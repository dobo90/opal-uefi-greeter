#![no_std]
#![no_main]
#![allow(clippy::missing_safety_doc)]

#[macro_use]
extern crate alloc;
// make sure to link this
extern crate rlibc;

use alloc::{string::String, vec::Vec};
use core::{convert::TryFrom, fmt::Write, ops::DerefMut, time::Duration};
use uefi::table::boot::LoadImageSource;

use uefi::{
    prelude::*,
    proto::{
        console::text::{Key, ScanCode},
        device_path::DevicePath,
        loaded_image::LoadedImage,
        media::{
            block::BlockIO,
            file::{File, FileAttribute, FileInfo, FileMode, FileType},
            fs::SimpleFileSystem,
            partition::{GptPartitionType, PartitionInfo},
        },
    },
    table::{boot::MemoryType, runtime::ResetType},
    CStr16, CString16,
};

use crate::{
    config::Config,
    error::{Error, OpalError, Result, ResultFixupExt},
    nvme_device::NvmeDevice,
    nvme_passthru::*,
    opal::{session::OpalSession, uid, LockingState, StatusCode},
    secure_device::SecureDevice,
    util::sleep,
};

pub mod config;
pub mod error;
pub mod nvme_device;
pub mod nvme_passthru;
pub mod opal;
pub mod secure_device;
pub mod util;

#[entry]
fn main(image_handle: Handle, mut st: SystemTable<Boot>) -> Status {
    if uefi_services::init(&mut st).is_err() {
        log::error!("Failed to initialize UEFI services");
        log::error!("Shutting down in 10s..");
        sleep(Duration::from_secs(10));
    }
    if let Err(err) = run(image_handle, &mut st) {
        log::error!("Error: {:?}", err);
        log::error!("Shutting down in 10s..");
        sleep(Duration::from_secs(10));
    }
    st.runtime_services()
        .reset(ResetType::SHUTDOWN, Status::SUCCESS, None)
}

fn run(image_handle: Handle, st: &mut SystemTable<Boot>) -> Result {
    config_stdout(st).fix(info!())?;

    let config = load_config(image_handle, st)?;

    let devices = find_secure_devices(st).fix(info!())?;

    for mut device in devices {
        if device.recv_locked().fix(info!())? {
            // session mutably borrows the device
            {
                let mut prompt = config.prompt.as_deref().unwrap_or("password: ");
                let mut session = loop {
                    let password = read_password(st, prompt)?;

                    let mut hash = vec![0; 32];

                    // as in sedutil-cli, maybe will change
                    pbkdf2::pbkdf2::<hmac::Hmac<sha1::Sha1>>(
                        password.as_bytes(),
                        device.proto().serial_num(),
                        75000,
                        &mut hash,
                    )
                    .unwrap();

                    if let Some(s) =
                        pretty_session(st, &mut device, &*hash, config.sed_locked_msg.as_deref())?
                    {
                        break s;
                    }

                    if config.clear_on_retry {
                        st.stdout().clear().fix(info!())?;
                    }

                    prompt = config
                        .retry_prompt
                        .as_deref()
                        .unwrap_or("bad password, retry: ");
                };

                session.set_mbr_done(true)?;
                session.set_locking_range(0, LockingState::ReadWrite)?;
            }

            // reconnect the controller to see
            // the real partition pop up after unlocking
            device.reconnect_controller(st).fix(info!())?;
        }
    }

    let handle = find_boot_partition(st)?;
    let agent = st.boot_services().image_handle();

    let dp = unsafe {
        st.boot_services().open_protocol::<DevicePath>(
            uefi::table::boot::OpenProtocolParams {
                handle: handle,
                agent: agent,
                controller: None,
            },
            uefi::table::boot::OpenProtocolAttributes::GetProtocol,
        )
    }
    .fix(info!())?;

    let image = CString16::try_from(config.image.as_str()).or(Err(Error::ConfigArgsBadUtf16))?;

    let buf = read_file(st, handle, &image)
        .fix(info!())?
        .ok_or(Error::ImageNotFound(config.image))?;

    if buf.get(0..2) != Some(&[0x4d, 0x5a]) {
        return Err(Error::ImageNotPeCoff);
    }

    let loaded_image_handle = st
        .boot_services()
        .load_image(
            image_handle,
            LoadImageSource::FromBuffer {
                file_path: Some(&dp),
                buffer: &buf,
            },
        )
        .fix(info!())?;

    let mut loaded_image = unsafe {
        st.boot_services().open_protocol::<LoadedImage>(
            uefi::table::boot::OpenProtocolParams {
                handle: loaded_image_handle,
                agent: agent,
                controller: None,
            },
            uefi::table::boot::OpenProtocolAttributes::GetProtocol,
        )
    }
    .fix(info!())?;

    let args = CString16::try_from(&*config.args).or(Err(Error::ConfigArgsBadUtf16))?;
    unsafe { loaded_image.set_load_options(args.as_ptr() as *const u8, args.num_bytes() as _) };

    st.boot_services()
        .start_image(loaded_image_handle)
        .fix(info!())?;

    Ok(())
}

fn config_stdout(st: &mut SystemTable<Boot>) -> uefi::Result {
    st.stdout().reset(false)?;

    if let Some(mode) = st.stdout().modes().max_by_key(|m| m.rows() * m.columns()) {
        st.stdout().set_mode(mode)?;
    };
    Ok(().into())
}

fn load_config(image_handle: Handle, st: &mut SystemTable<Boot>) -> Result<Config> {
    let agent = st.boot_services().image_handle();

    let loaded_image = unsafe {
        st.boot_services().open_protocol::<LoadedImage>(
            uefi::table::boot::OpenProtocolParams {
                handle: image_handle,
                agent: agent,
                controller: None,
            },
            uefi::table::boot::OpenProtocolAttributes::GetProtocol,
        )
    }
    .fix(info!())?;

    let device_path = unsafe {
        st.boot_services().open_protocol::<DevicePath>(
            uefi::table::boot::OpenProtocolParams {
                handle: loaded_image.device(),
                agent: agent,
                controller: None,
            },
            uefi::table::boot::OpenProtocolAttributes::GetProtocol,
        )
    }
    .fix(info!())?;

    let device_handle = st
        .boot_services()
        .locate_device_path::<SimpleFileSystem>(&mut &*device_path)
        .fix(info!())?;
    let buf = read_file(st, device_handle, cstr16!("config"))
        .fix(info!())?
        .ok_or(Error::ConfigMissing)?;
    let config = Config::parse(&buf)?;
    log::set_max_level(config.log_level);
    log::debug!("loaded config = {:#?}", config);
    Ok(config)
}

fn write_char(st: &mut SystemTable<Boot>, ch: u16) -> Result {
    let str = &[ch, 0];
    st.stdout()
        .output_string(unsafe { CStr16::from_u16_with_nul_unchecked(str) })
        .fix(info!())
}

fn read_password(st: &mut SystemTable<Boot>, prompt: &str) -> Result<String> {
    st.stdout().write_str(prompt).unwrap();

    let mut wait_for_key = [unsafe { st.stdin().wait_for_key_event().unsafe_clone() }];

    let mut data = String::with_capacity(32);
    loop {
        st.boot_services()
            .wait_for_event(&mut wait_for_key)
            .fix(info!())?;

        match st.stdin().read_key().fix(info!())? {
            Some(Key::Printable(k)) if [0xD, 0xA].contains(&u16::from(k)) => {
                write_char(st, 0x0D)?;
                write_char(st, 0x0A)?;
                break Ok(data);
            }
            Some(Key::Printable(k)) if u16::from(k) == 0x8 => {
                if data.pop().is_some() {
                    write_char(st, 0x08)?;
                }
            }
            Some(Key::Printable(k)) => {
                write_char(st, '*' as u16)?;
                data.push(k.into());
            }
            Some(Key::Special(ScanCode::ESCAPE)) => {
                st.runtime_services()
                    .reset(ResetType::SHUTDOWN, Status::SUCCESS, None)
            }
            _ => {}
        }
    }
}

fn pretty_session<'d>(
    st: &mut SystemTable<Boot>,
    device: &'d mut SecureDevice,
    challenge: &[u8],
    sed_locked_msg: Option<&str>,
) -> Result<Option<OpalSession<'d>>> {
    match OpalSession::start(
        device,
        uid::OPAL_LOCKINGSP,
        uid::OPAL_ADMIN1,
        Some(challenge),
    ) {
        Ok(session) => Ok(Some(session)),
        Err(Error::Opal(OpalError::Status(StatusCode::NOT_AUTHORIZED))) => Ok(None),
        Err(Error::Opal(OpalError::Status(StatusCode::AUTHORITY_LOCKED_OUT))) => {
            st.stdout()
                .write_str(
                    sed_locked_msg
                        .unwrap_or("Too many bad tries, SED locked out, resetting in 10s.."),
                )
                .unwrap();
            sleep(Duration::from_secs(10));
            st.runtime_services()
                .reset(ResetType::COLD, Status::WARN_RESET_REQUIRED, None);
        }
        e => e.map(Some),
    }
}

fn find_secure_devices(st: &mut SystemTable<Boot>) -> uefi::Result<Vec<SecureDevice>> {
    let mut result = Vec::new();

    let agent = st.boot_services().image_handle();

    for handle in st.boot_services().find_handles::<BlockIO>()? {
        let blockio = unsafe {
            st.boot_services().open_protocol::<BlockIO>(
                uefi::table::boot::OpenProtocolParams {
                    handle: handle,
                    agent: agent,
                    controller: None,
                },
                uefi::table::boot::OpenProtocolAttributes::GetProtocol,
            )
        }?;

        if blockio.media().is_logical_partition() {
            continue;
        }

        let device_path = unsafe {
            st.boot_services().open_protocol::<DevicePath>(
                uefi::table::boot::OpenProtocolParams {
                    handle: handle,
                    agent: agent,
                    controller: None,
                },
                uefi::table::boot::OpenProtocolAttributes::GetProtocol,
            )
        }?;

        if let Ok(nvme) = st
            .boot_services()
            .locate_device_path::<NvmExpressPassthru>(&mut &*device_path)
        {
            let mut nvme = unsafe {
                st.boot_services().open_protocol::<NvmExpressPassthru>(
                    uefi::table::boot::OpenProtocolParams {
                        handle: nvme,
                        agent: agent,
                        controller: None,
                    },
                    uefi::table::boot::OpenProtocolAttributes::GetProtocol,
                )
            }?;

            let nvme = nvme.deref_mut();

            result.push(SecureDevice::new(handle, NvmeDevice::new(nvme)?)?)
        }

        // todo something like that:
        //
        // if let Ok(ata) = st
        //     .boot_services()
        //     .locate_device_path::<AtaExpressPassthru>(device_path)
        //     .log_warning()
        // {
        //     let ata = st
        //         .boot_services()
        //         .handle_protocol::<AtaExpressPassthru>(ata)?
        //         .log();
        //
        //     result.push(SecureDevice::new(handle, AtaDevice::new(ata.get())?.log())?.log())
        // }
        //
        // ..etc
    }
    Ok(result.into())
}

fn find_boot_partition(st: &mut SystemTable<Boot>) -> Result<Handle> {
    let mut res = None;
    for handle in st
        .boot_services()
        .find_handles::<PartitionInfo>()
        .fix(info!())?
    {
        let pi = unsafe {
            st.boot_services().open_protocol::<PartitionInfo>(
                uefi::table::boot::OpenProtocolParams {
                    handle: handle,
                    agent: st.boot_services().image_handle(),
                    controller: None,
                },
                uefi::table::boot::OpenProtocolAttributes::GetProtocol,
            )
        }
        .fix(info!())?;

        match pi.gpt_partition_entry() {
            Some(gpt) if { gpt.partition_type_guid } == GptPartitionType::EFI_SYSTEM_PARTITION => {
                if res.replace(handle).is_some() {
                    return Err(Error::MultipleBootPartitions);
                }
            }
            _ => {}
        }
    }
    res.ok_or(Error::NoBootPartitions)
}

fn read_file(
    st: &SystemTable<Boot>,
    device: Handle,
    file: &CStr16,
) -> uefi::Result<Option<Vec<u8>>> {
    let mut sfs = unsafe {
        st.boot_services().open_protocol::<SimpleFileSystem>(
            uefi::table::boot::OpenProtocolParams {
                handle: device,
                agent: st.boot_services().image_handle(),
                controller: None,
            },
            uefi::table::boot::OpenProtocolAttributes::GetProtocol,
        )
    }?;

    let file_handle = sfs
        .open_volume()?
        .open(&file, FileMode::Read, FileAttribute::empty())?;

    if let FileType::Regular(mut f) = file_handle.into_type()? {
        let info = f.get_boxed_info::<FileInfo>()?;
        let size = info.file_size() as usize;
        let ptr = st
            .boot_services()
            .allocate_pool(MemoryType::LOADER_DATA, size)?;
        let mut buf = unsafe { Vec::from_raw_parts(ptr, size, size) };

        let read = f
            .read(&mut buf)
            .map_err(|_| uefi::Status::BUFFER_TOO_SMALL)?;
        buf.truncate(read);
        Ok(Some(buf).into())
    } else {
        Ok(None.into())
    }
}
