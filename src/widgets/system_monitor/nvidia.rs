use std::{
    ffi::{CStr, CString, c_char, c_uint, c_void},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use libloading::Library;

use super::{
    GpuSnapshot,
    gpu::{DataState, GpuRequest},
    read_trimmed,
};

type Device = *mut c_void;
type Status = c_uint;
type Initialize = unsafe extern "C" fn() -> Status;
type Shutdown = unsafe extern "C" fn() -> Status;
type DeviceByPci = unsafe extern "C" fn(*const c_char, *mut Device) -> Status;
type DeviceName = unsafe extern "C" fn(Device, *mut c_char, c_uint) -> Status;
type DeviceUtilization = unsafe extern "C" fn(Device, *mut Utilization) -> Status;
type DeviceMemory = unsafe extern "C" fn(Device, *mut Memory) -> Status;
type DevicePower = unsafe extern "C" fn(Device, *mut c_uint) -> Status;
type DeviceTemperature = unsafe extern "C" fn(Device, c_uint, *mut c_uint) -> Status;

#[repr(C)]
#[derive(Default)]
struct Utilization {
    gpu: c_uint,
    memory: c_uint,
}

#[repr(C)]
#[derive(Default)]
struct Memory {
    total: u64,
    free: u64,
    used: u64,
}

struct Nvml {
    _library: Arc<Library>,
    initialized: bool,
    shutdown: Shutdown,
    device_by_pci: DeviceByPci,
    name: DeviceName,
    utilization: DeviceUtilization,
    memory: DeviceMemory,
    power: DevicePower,
    temperature: DeviceTemperature,
    frequency: DeviceTemperature,
}

impl Nvml {
    fn open(enabled: &AtomicBool) -> Result<Self, DataState> {
        // NVML is optional. Use the system driver, including NixOS's driver path.
        // Signatures and repr(C) layouts follow the NVML API:
        // https://docs.nvidia.com/deploy/nvml-api/api/group__nvmlDeviceQueries.html
        unsafe {
            let library = nvml_library()?;
            let initialize = *library
                .get::<Initialize>(b"nvmlInit_v2\0")
                .map_err(|_| DataState::Unsupported)?;
            let mut api = Self {
                initialized: false,
                shutdown: *library
                    .get(b"nvmlShutdown\0")
                    .map_err(|_| DataState::Unsupported)?,
                device_by_pci: *library
                    .get(b"nvmlDeviceGetHandleByPciBusId_v2\0")
                    .map_err(|_| DataState::Unsupported)?,
                name: *library
                    .get(b"nvmlDeviceGetName\0")
                    .map_err(|_| DataState::Unsupported)?,
                utilization: *library
                    .get(b"nvmlDeviceGetUtilizationRates\0")
                    .map_err(|_| DataState::Unsupported)?,
                memory: *library
                    .get(b"nvmlDeviceGetMemoryInfo\0")
                    .map_err(|_| DataState::Unsupported)?,
                power: *library
                    .get(b"nvmlDeviceGetPowerUsage\0")
                    .map_err(|_| DataState::Unsupported)?,
                temperature: *library
                    .get(b"nvmlDeviceGetTemperature\0")
                    .map_err(|_| DataState::Unsupported)?,
                frequency: *library
                    .get(b"nvmlDeviceGetClockInfo\0")
                    .map_err(|_| DataState::Unsupported)?,
                _library: library,
            };
            // v2 initializes lazily. Only snapshot() acquires the specific,
            // already-active PCI device; never enumerate/attach all GPUs.
            if !enabled.load(Ordering::Acquire) {
                return Err(DataState::Paused);
            }
            nvml_result(initialize())?;
            api.initialized = true;
            Ok(api)
        }
    }

    fn snapshot(
        &self,
        pci: &CStr,
        enabled: &AtomicBool,
        request: GpuRequest,
    ) -> Result<GpuSnapshot, DataState> {
        let mut device = std::ptr::null_mut();
        // Buffers follow NVML's C ABI and live throughout each synchronous call.
        unsafe {
            if !enabled.load(Ordering::Acquire) {
                return Err(DataState::Paused);
            }
            nvml_result((self.device_by_pci)(pci.as_ptr(), &mut device))?;
            if device.is_null() {
                return Err(DataState::Unavailable);
            }
            let mut data = GpuSnapshot::default();
            let mut name = [0_u8; 256];
            if (self.name)(device, name.as_mut_ptr().cast(), name.len() as c_uint) == 0 {
                data.name = CStr::from_bytes_until_nul(&name)
                    .ok()
                    .map(|name| name.to_string_lossy().into_owned());
            }
            let mut query = |key, requested: bool, result: &mut dyn FnMut() -> Status| {
                let state = if !requested || !enabled.load(Ordering::Acquire) {
                    Err(DataState::Paused)
                } else {
                    nvml_result(result())
                };
                if let Err(state) = state {
                    data.states.insert(key, state);
                }
                state.is_ok()
            };
            let mut utilization = Utilization::default();
            if query("load", request.load, &mut || {
                (self.utilization)(device, &mut utilization)
            }) && utilization.gpu <= 100
            {
                data.utilization_percent = Some(f64::from(utilization.gpu));
            }
            let mut memory = Memory::default();
            if query("memory-used", request.memory, &mut || {
                (self.memory)(device, &mut memory)
            }) && memory.total > 0
            {
                data.memory_used = Some(memory.used);
                data.memory_total = Some(memory.total);
            }
            let mut power = 0;
            if query("power", request.power, &mut || {
                (self.power)(device, &mut power)
            }) {
                data.power_watts = Some(f64::from(power) / 1000.0);
            }
            let mut temperature = 0;
            if query("temperature", request.temperature, &mut || {
                (self.temperature)(device, 0, &mut temperature)
            }) && temperature <= 200
            {
                data.temperature_celsius = Some(f64::from(temperature));
            }
            let mut frequency = 0;
            if query("frequency", request.frequency, &mut || {
                (self.frequency)(device, 0, &mut frequency)
            }) {
                data.frequency_mhz = Some(f64::from(frequency));
            }
            if let Some(state) = data.states.get("memory-used").copied() {
                data.states.insert("memory-total", state);
            }
            Ok(data)
        }
    }
}

fn nvml_library() -> Result<Arc<Library>, DataState> {
    static LIBRARY: Mutex<Option<Arc<Library>>> = Mutex::new(None);
    let mut cached = LIBRARY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(library) = cached.as_ref() {
        return Ok(Arc::clone(library));
    }
    // Repeated dlopen/dlclose leaks an eventfd per load in NVIDIA's library.
    // Retain only the code mapping; each Nvml session still calls Shutdown to
    // release driver handles, so an idle GPU can enter runtime suspension.
    let library = unsafe {
        Library::new("libnvidia-ml.so.1")
            .or_else(|_| Library::new("/run/opengl-driver/lib/libnvidia-ml.so.1"))
    }
    .map_err(|_| DataState::DriverMissing)?;
    let library = Arc::new(library);
    *cached = Some(Arc::clone(&library));
    Ok(library)
}

impl Drop for Nvml {
    fn drop(&mut self) {
        // Do not retain device handles between samples: they can prevent runtime
        // suspension on hybrid-graphics laptops.
        if self.initialized {
            unsafe {
                (self.shutdown)();
            }
        }
    }
}

fn can_sample(device: &Path, enabled: &AtomicBool) -> bool {
    enabled.load(Ordering::Acquire)
        && read_trimmed(device.join("power/runtime_status")).as_deref() == Some("active")
}

fn nvml_result(status: Status) -> Result<(), DataState> {
    match status {
        0 => Ok(()),
        3 => Err(DataState::Unsupported),
        4 => Err(DataState::PermissionDenied),
        9 | 18 => Err(DataState::DriverMissing),
        _ => Err(DataState::Unavailable),
    }
}

pub(super) fn read_snapshot(
    device: &Path,
    enabled: &AtomicBool,
    request: GpuRequest,
) -> Result<GpuSnapshot, DataState> {
    if !enabled.load(Ordering::Acquire) || !request.any() {
        return Err(DataState::Paused);
    }
    if !can_sample(device, enabled) {
        return Err(
            if matches!(
                read_trimmed(device.join("power/runtime_status")).as_deref(),
                Some("suspended" | "suspending")
            ) {
                DataState::Sleeping
            } else {
                DataState::Unavailable
            },
        );
    }
    let device = std::fs::canonicalize(device).map_err(|_| DataState::Unavailable)?;
    let pci = CString::new(
        device
            .file_name()
            .ok_or(DataState::Unavailable)?
            .as_encoded_bytes(),
    )
    .map_err(|_| DataState::Unavailable)?;
    let api = Nvml::open(enabled)?;
    if !can_sample(&device, enabled) {
        return Err(DataState::Sleeping);
    }
    api.snapshot(&pci, enabled, request)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_metrics() -> GpuRequest {
        GpuRequest {
            load: true,
            memory: true,
            temperature: true,
            power: true,
            frequency: true,
        }
    }

    #[test]
    fn disabled_or_sleeping_gpu_is_skipped_before_loading_nvml() {
        let root =
            std::env::temp_dir().join(format!("obsidian-nvidia-sleep-test-{}", std::process::id()));
        std::fs::create_dir_all(root.join("power")).unwrap();
        let enabled = AtomicBool::new(true);
        for state in ["suspended", "suspending", "resuming", "unsupported"] {
            std::fs::write(root.join("power/runtime_status"), state).unwrap();
            assert!(!can_sample(&root, &enabled));
            assert!(read_snapshot(&root, &enabled, all_metrics()).is_err());
        }
        std::fs::write(root.join("power/runtime_status"), "active").unwrap();
        assert!(can_sample(&root, &enabled));
        enabled.store(false, Ordering::Release);
        assert!(!can_sample(&root, &enabled));
        assert!(read_snapshot(&root, &enabled, all_metrics()).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires access to an active NVIDIA GPU"]
    fn active_nvidia_reports_metrics_and_releases_the_session() {
        let enabled = AtomicBool::new(true);
        let device = super::super::sorted_directory_paths(Path::new("/sys/class/drm"))
            .into_iter()
            .map(|card| card.join("device"))
            .find(|device| super::super::device_is_nvidia(device) && can_sample(device, &enabled))
            .expect("an active NVIDIA GPU");
        for _ in 0..2 {
            let snapshot =
                read_snapshot(&device, &enabled, all_metrics()).expect("NVIDIA snapshot");
            eprintln!("NVIDIA metrics: {snapshot:?}");
            assert!(snapshot.name.is_some());
            assert!(snapshot.utilization_percent.is_some());
            assert!(snapshot.memory_total.is_some());
            assert!(snapshot.temperature_celsius.is_some());
        }
        let descriptors = || std::fs::read_dir("/proc/self/fd").unwrap().count();
        let before = descriptors();
        for _ in 0..32 {
            read_snapshot(&device, &enabled, all_metrics()).expect("repeated NVIDIA sample");
        }
        let after = descriptors();
        eprintln!("NVIDIA descriptors: {before} -> {after}");
        assert!(
            after <= before + 1,
            "sampling must release its file descriptors"
        );
        let held_devices = std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .filter(|path| {
                path.to_string_lossy()
                    .strip_prefix("/dev/nvidia")
                    .is_some_and(|suffix| {
                        !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit())
                    })
            })
            .collect::<Vec<_>>();
        assert!(
            held_devices.is_empty(),
            "shutdown must release GPU devices: {held_devices:?}"
        );
        enabled.store(false, Ordering::Release);
        assert!(read_snapshot(&device, &enabled, all_metrics()).is_err());
    }
}
