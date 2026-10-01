//! nvidia-drm and NVKMS permission ioctls for the direct display path.
//!
//! When a Vulkan client acquires displays through nvidia-drm
//! (`vkAcquireDrmDisplayEXT`), the NVIDIA driver's own NVKMS handle only
//! receives per-head modeset permission. NVKMS reserves several operations
//! for the modeset owner or sub-owner: the swap group behind
//! `VK_NV_present_barrier`, flip-lock groups, and framelock attributes. A DRM
//! master may hand out one sub-owner grant, and nothing stops it from handing
//! that grant to the driver handle inside its own process.
//!
//! Sub-ownership also switches nvidia-drm into a passive mode: it stops
//! handling NVKMS flip events and rejects every DRM atomic commit until the
//! grant is revoked. That removes the `nv_flip == NULL` kernel warnings and
//! the per-head three-second flip timeouts that otherwise accompany direct
//! presentation, but it also means a grant left behind by a crashed process
//! blocks any compositor until some DRM master revokes it.
//!
//! Layouts follow the open-gpu-kernel-modules headers `nvidia-drm-ioctl.h`,
//! `nvkms-ioctl.h`, and `nvkms-api.h`; the permission ABI has been stable
//! since it was introduced and the structs match the 550 through 615 series.
//! The NVKMS command index of `ACQUIRE_PERMISSIONS` is not stable: it is 41 on
//! 580 through 610 and 40 on 550 and 615, which lack `CHECK_LUT_NOTIFIER`.

use anyhow::{anyhow, Context, Result};
use std::{
    fs::OpenOptions,
    os::fd::{AsRawFd, RawFd},
};

const DRM_IOCTL_BASE: u64 = b'd' as u64;
const DRM_COMMAND_BASE: u64 = 0x40;
const DRM_NVIDIA_GRANT_PERMISSIONS: u64 = 0x12;
const DRM_NVIDIA_REVOKE_PERMISSIONS: u64 = 0x13;
const NV_DRM_PERMISSIONS_TYPE_SUB_OWNER: u32 = 3;
const NVKMS_IOCTL_MAGIC: u64 = b'm' as u64;
/// `ACQUIRE_PERMISSIONS` command index on 580 through 610.
const NVKMS_IOCTL_ACQUIRE_PERMISSIONS_WITH_LUT_NOTIFIER: u32 = 41;
/// `ACQUIRE_PERMISSIONS` command index on 550 and 615 (no `CHECK_LUT_NOTIFIER`).
const NVKMS_IOCTL_ACQUIRE_PERMISSIONS_WITHOUT_LUT_NOTIFIER: u32 = 40;
const NVKMS_DEVICE: &str = "/dev/nvidia-modeset";
/// The DRM driver name nvidia-drm reports through `DRM_IOCTL_VERSION`.
const NVIDIA_DRM_NAME: &str = "nvidia-drm";

/// Why [`grant_sub_ownership_to_driver`] failed, split by whether the
/// nvidia-drm grant had taken effect.
#[derive(Debug)]
pub enum GrantFailure {
    /// The DRM grant was refused (or never attempted); nothing was revoked, so
    /// the driver keeps the per-head modeset permission it acquired.
    NotGranted(anyhow::Error),
    /// The grant succeeded, no handle accepted the token, and the grant was
    /// revoked. NVKMS revoke strips modeset permission from every client except
    /// nvidia-drm, including the per-head permission the Vulkan driver acquired,
    /// so direct presentation cannot work afterwards.
    RevokedAfterGrant(anyhow::Error),
}

impl std::fmt::Display for GrantFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotGranted(error) | Self::RevokedAfterGrant(error) => write!(f, "{error:#}"),
        }
    }
}

impl std::error::Error for GrantFailure {}

/// `ACQUIRE_PERMISSIONS` command indices to try, most likely first for the
/// running driver version (`None` when it cannot be read). Major 615 and later
/// and majors before 580 use 40; 580 through 610 and unknown use 41.
fn acquire_permissions_commands(driver_version: Option<&str>) -> [u32; 2] {
    const WITH: u32 = NVKMS_IOCTL_ACQUIRE_PERMISSIONS_WITH_LUT_NOTIFIER;
    const WITHOUT: u32 = NVKMS_IOCTL_ACQUIRE_PERMISSIONS_WITHOUT_LUT_NOTIFIER;
    let major = driver_version
        .and_then(|version| version.split('.').next())
        .and_then(|major| major.parse::<u32>().ok());
    match major {
        Some(major) if !(580..615).contains(&major) => [WITHOUT, WITH],
        _ => [WITH, WITHOUT],
    }
}

#[repr(C)]
struct DrmVersion {
    version_major: i32,
    version_minor: i32,
    version_patchlevel: i32,
    name_len: usize,
    name: *mut u8,
    date_len: usize,
    date: *mut u8,
    desc_len: usize,
    desc: *mut u8,
}

#[repr(C)]
struct DrmGrantPermissions {
    fd: i32,
    dpy_id: u32,
    kind: u32,
}

#[repr(C)]
struct DrmRevokePermissions {
    dpy_id: u32,
    kind: u32,
}

#[repr(C)]
struct NvKmsIoctlParams {
    cmd: u32,
    size: u32,
    address: u64,
}

/// `struct NvKmsAcquirePermissionsParams`: request `{ int fd; }` followed by
/// reply `{ NvU32 deviceHandle; struct NvKmsPermissions permissions; }`, whose
/// union spans four heads of `NVDpyIdList`.
#[repr(C)]
struct NvKmsAcquirePermissionsParams {
    fd: i32,
    device_handle: u32,
    permissions_type: u32,
    permissions_union: [u32; 4],
}

const fn iowr(magic: u64, nr: u64, size: usize) -> libc::c_ulong {
    ((3u64 << 30) | ((size as u64) << 16) | (magic << 8) | nr) as libc::c_ulong
}

fn drm_ioctl<T>(fd: RawFd, nr: u64, params: &mut T) -> std::io::Result<()> {
    let request = iowr(DRM_IOCTL_BASE, nr, std::mem::size_of::<T>());
    // SAFETY: `params` is a live `#[repr(C)]` value of the size encoded in the
    // request; the kernel reads and writes only within it.
    let rc = unsafe { libc::ioctl(fd, request, params as *mut T) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn nvkms_ioctl<T>(fd: RawFd, cmd: u32, params: &mut T) -> std::io::Result<()> {
    let mut wrapper = NvKmsIoctlParams {
        cmd,
        size: std::mem::size_of::<T>() as u32,
        address: params as *mut T as u64,
    };
    let request = iowr(
        NVKMS_IOCTL_MAGIC,
        0,
        std::mem::size_of::<NvKmsIoctlParams>(),
    );
    // SAFETY: as above; NVKMS copies `size` bytes at `address`, which is
    // exactly the `#[repr(C)]` value borrowed for this call.
    let rc = unsafe { libc::ioctl(fd, request, &mut wrapper as *mut NvKmsIoctlParams) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// The kernel DRM driver name behind `fd`, from `DRM_IOCTL_VERSION`.
pub fn drm_driver_name(fd: RawFd) -> std::io::Result<String> {
    let mut version = DrmVersion {
        version_major: 0,
        version_minor: 0,
        version_patchlevel: 0,
        name_len: 0,
        name: std::ptr::null_mut(),
        date_len: 0,
        date: std::ptr::null_mut(),
        desc_len: 0,
        desc: std::ptr::null_mut(),
    };
    drm_ioctl(fd, 0x00, &mut version)?;
    let mut name = vec![0u8; version.name_len.min(256)];
    version.name_len = name.len();
    version.name = name.as_mut_ptr();
    version.date_len = 0;
    version.desc_len = 0;
    drm_ioctl(fd, 0x00, &mut version)?;
    name.truncate(version.name_len.min(name.len()));
    Ok(String::from_utf8_lossy(&name).into_owned())
}

/// Whether `fd` is an nvidia-drm card. The vendor ioctl numbers below mean
/// something else on every other DRM driver, so callers must check first.
pub fn is_nvidia_drm(fd: RawFd) -> bool {
    drm_driver_name(fd).is_ok_and(|name| name == NVIDIA_DRM_NAME)
}

/// File descriptors in this process that are opens of the NVKMS device, i.e.
/// the NVIDIA driver's own handles.
fn driver_nvkms_fds(exclude: RawFd) -> Result<Vec<RawFd>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc/self/fd").context("listing /proc/self/fd")? {
        let entry = entry?;
        let Ok(fd) = entry.file_name().to_string_lossy().parse::<RawFd>() else {
            continue;
        };
        if fd == exclude {
            continue;
        }
        if let Ok(target) = std::fs::read_link(entry.path()) {
            if target.as_os_str() == NVKMS_DEVICE {
                found.push(fd);
            }
        }
    }
    found.sort_unstable();
    Ok(found)
}

/// Grant NVKMS sub-ownership from the DRM master `card_fd` to the NVIDIA
/// driver's NVKMS device handle in this process and return that handle's fd.
///
/// Call this after every display has been acquired: with full permissions
/// already held, `vkAcquireDrmDisplayEXT` fails with
/// `VK_ERROR_INITIALIZATION_FAILED`. On any failure after the grant took
/// effect, the grant is revoked before the error is returned.
pub fn grant_sub_ownership_to_driver(card_fd: RawFd) -> Result<RawFd, GrantFailure> {
    let commands = acquire_permissions_commands(
        crate::nvidia_driver::detect()
            .as_ref()
            .map(|status| status.version.as_str()),
    );
    grant_with_commands(card_fd, commands)
}

fn grant_with_commands(card_fd: RawFd, commands: [u32; 2]) -> Result<RawFd, GrantFailure> {
    if !is_nvidia_drm(card_fd) {
        return Err(GrantFailure::NotGranted(anyhow!(
            "card is not driven by nvidia-drm"
        )));
    }
    let token = OpenOptions::new()
        .read(true)
        .write(true)
        .open(NVKMS_DEVICE)
        .with_context(|| format!("opening {NVKMS_DEVICE} for the grant token"))
        .map_err(GrantFailure::NotGranted)?;
    let candidates = driver_nvkms_fds(token.as_raw_fd()).map_err(GrantFailure::NotGranted)?;
    if candidates.is_empty() {
        return Err(GrantFailure::NotGranted(anyhow!("the NVIDIA driver holds no {NVKMS_DEVICE} handle in this process; create the Vulkan instance first")));
    }
    let mut grant = DrmGrantPermissions {
        fd: token.as_raw_fd(),
        dpy_id: 0,
        kind: NV_DRM_PERMISSIONS_TYPE_SUB_OWNER,
    };
    drm_ioctl(
        card_fd,
        DRM_COMMAND_BASE + DRM_NVIDIA_GRANT_PERMISSIONS,
        &mut grant,
    )
    .context(
        "DRM_IOCTL_NVIDIA_GRANT_PERMISSIONS(SUB_OWNER); needs DRM master and no existing grant",
    )
    .map_err(GrantFailure::NotGranted)?;
    let mut acquired = None;
    let mut failures = Vec::new();
    // A wrong command index is harmless: it lands on GRANT_PERMISSIONS or
    // REVOKE_PERMISSIONS, whose 32-byte params never match this 28-byte
    // request, so `nvKmsIoctl` rejects it on its `paramSize` check before any
    // handler runs.
    'search: for fd in candidates {
        for cmd in commands {
            let mut params = NvKmsAcquirePermissionsParams {
                fd: token.as_raw_fd(),
                device_handle: 0,
                permissions_type: 0,
                permissions_union: [0; 4],
            };
            match nvkms_ioctl(fd, cmd, &mut params) {
                Ok(()) => {
                    acquired = Some(fd);
                    break 'search;
                }
                Err(err) => failures.push(format!("fd {fd} cmd {cmd}: {err}")),
            }
        }
    }
    drop(token);
    match acquired {
        Some(fd) => Ok(fd),
        None => {
            let revoke = revoke_sub_ownership(card_fd)
                .map(|_| "grant revoked".to_string())
                .unwrap_or_else(|err| format!("grant revoke failed: {err:#}"));
            Err(GrantFailure::RevokedAfterGrant(anyhow!(
                "no NVKMS handle accepted the sub-ownership token ({}); {revoke}",
                failures.join("; ")
            )))
        }
    }
}

/// Revoke sub-ownership on `card_fd`, which must be DRM master. NVKMS shuts
/// every head down as part of this, and nvidia-drm resumes accepting atomic
/// commits and flip events afterwards.
pub fn revoke_sub_ownership(card_fd: RawFd) -> Result<()> {
    let mut revoke = DrmRevokePermissions {
        dpy_id: 0,
        kind: NV_DRM_PERMISSIONS_TYPE_SUB_OWNER,
    };
    drm_ioctl(
        card_fd,
        DRM_COMMAND_BASE + DRM_NVIDIA_REVOKE_PERMISSIONS,
        &mut revoke,
    )
    .context("DRM_IOCTL_NVIDIA_REVOKE_PERMISSIONS(SUB_OWNER)")
}

/// Revoke any grant a previous process may have left behind. Returns
/// `Ok(true)` when nvidia-drm accepted the revoke (current kernels report
/// success whether or not a grant existed, so this does not prove one did)
/// and `Ok(false)` when the driver answered `EINVAL` or the card is not driven
/// by nvidia-drm.
pub fn clear_stale_sub_ownership(card_fd: RawFd) -> Result<bool> {
    if !is_nvidia_drm(card_fd) {
        return Ok(false);
    }
    match revoke_sub_ownership(card_fd) {
        Ok(()) => Ok(true),
        Err(err)
            if err
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.raw_os_error() == Some(libc::EINVAL)) =>
        {
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_layouts_match_the_kernel_headers() {
        assert_eq!(std::mem::size_of::<DrmVersion>(), 64);
        assert_eq!(std::mem::size_of::<DrmGrantPermissions>(), 12);
        assert_eq!(std::mem::size_of::<DrmRevokePermissions>(), 8);
        assert_eq!(std::mem::size_of::<NvKmsIoctlParams>(), 16);
        // Observed on the wire as size=28 for NVKMS_IOCTL_ACQUIRE_PERMISSIONS.
        assert_eq!(std::mem::size_of::<NvKmsAcquirePermissionsParams>(), 28);
    }

    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        // _IOWR('m', 0, struct NvKmsIoctlParams)
        assert_eq!(
            iowr(
                NVKMS_IOCTL_MAGIC,
                0,
                std::mem::size_of::<NvKmsIoctlParams>()
            ),
            0xC010_6D00
        );
        // DRM_IOWR(DRM_COMMAND_BASE + 0x12, struct drm_nvidia_grant_permissions_params)
        assert_eq!(
            iowr(
                DRM_IOCTL_BASE,
                DRM_COMMAND_BASE + DRM_NVIDIA_GRANT_PERMISSIONS,
                std::mem::size_of::<DrmGrantPermissions>()
            ),
            0xC00C_6452
        );
        assert_eq!(
            iowr(DRM_IOCTL_BASE, 0, std::mem::size_of::<DrmVersion>()),
            0xC040_6400
        );
    }

    #[test]
    fn acquire_command_index_follows_the_driver_release() {
        // 41 on 580..=610, 40 on 550 and 615 (no CHECK_LUT_NOTIFIER).
        assert_eq!(NVKMS_IOCTL_ACQUIRE_PERMISSIONS_WITH_LUT_NOTIFIER, 41);
        assert_eq!(NVKMS_IOCTL_ACQUIRE_PERMISSIONS_WITHOUT_LUT_NOTIFIER, 40);
        for (version, order) in [
            (Some("550.163.01"), [40, 41]),
            (Some("579.99"), [40, 41]),
            (Some("580.1.0"), [41, 40]),
            (Some("595.91.07"), [41, 40]),
            (Some("610.57.04"), [41, 40]),
            (Some("615.71.09"), [40, 41]),
            (Some("620.1"), [40, 41]),
            (Some("garbage"), [41, 40]),
            (None, [41, 40]),
        ] {
            assert_eq!(acquire_permissions_commands(version), order, "{version:?}");
        }
    }

    #[test]
    fn non_drm_descriptors_are_not_nvidia() {
        let file = std::fs::File::open("/dev/null").unwrap();
        assert!(!is_nvidia_drm(file.as_raw_fd()));
        assert!(!clear_stale_sub_ownership(file.as_raw_fd()).unwrap());
        assert!(matches!(
            grant_with_commands(file.as_raw_fd(), [41, 40]),
            Err(GrantFailure::NotGranted(_))
        ));
    }
}
