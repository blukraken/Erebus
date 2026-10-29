//! The vitals sampler. A dedicated thread reads RAM, per-core CPU and NVIDIA GPU state once a
//! second, on the wall-clock second, and hands each [`Sample`] to the hub.
//!
//! CPU comes from `NtQuerySystemInformation` counter deltas, RAM from `GlobalMemoryStatusEx`,
//! and the GPU from `nvml.dll`, loaded at run time. A machine without NVML reports `gpu: None`.
//! `EREBUS_NVML=0` skips NVML entirely.
//!
//! Debug builds read `EREBUS_FAULT` once at start, a comma-separated list of fault tokens.
//! Release builds ignore it.
//!
//! - `nvml-dll` acts as if `nvml.dll` is not found.
//! - `nvml-init` acts as if `nvmlInit_v2` failed.
//! - `gpu@N`, `ram@N` and `cpu@N` fail that read on loop iterations N, N+1 and N+2
//!   (1-based), then recover.

use std::ffi::{c_char, c_void, CStr};
use std::mem::size_of;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use erebus_protocol::{GpuInfo, GpuSample, Hardware, Sample};
use tokio::sync::mpsc::UnboundedSender;
use windows_sys::Win32::Foundation::FreeLibrary;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

use crate::hub::Event;
use crate::log::{log_info, log_warn};

const SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION: u32 = 8;
const MAX_CORES: usize = 256;
const MB: u64 = 1024 * 1024;

#[link(name = "ntdll")]
extern "system" {
    fn NtQuerySystemInformation(class: u32, info: *mut c_void, len: u32, ret_len: *mut u32) -> i32;
}

/// `SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawPerf {
    idle: i64,
    kernel: i64,
    user: i64,
    dpc: i64,
    interrupt: i64,
    interrupt_count: u32,
    _pad: u32,
}

/// One core's cumulative counters. `kernel` includes `idle`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub idle: u64,
    pub kernel: u64,
    pub user: u64,
}

/// Busy percentage (0..=100) of one core between two counter reads.
pub fn core_percent(prev: Counters, cur: Counters) -> f32 {
    let total = cur.kernel.wrapping_add(cur.user).wrapping_sub(prev.kernel.wrapping_add(prev.user));
    let idle = cur.idle.wrapping_sub(prev.idle);
    // A backwards counter wraps to a huge delta; treat it as no data.
    if total == 0 || total > i64::MAX as u64 || idle > total {
        return 0.0;
    }
    ((total - idle) as f64 / total as f64 * 100.0).clamp(0.0, 100.0) as f32
}

/// Per-core and mean busy percentages. Cores missing from either read are dropped.
pub fn cpu_percent(prev: &[Counters], cur: &[Counters]) -> (f32, Vec<u8>) {
    let pct: Vec<f32> = prev.iter().zip(cur).map(|(p, c)| core_percent(*p, *c)).collect();
    if pct.is_empty() {
        return (0.0, Vec::new());
    }
    let mean = pct.iter().sum::<f32>() / pct.len() as f32;
    (mean.clamp(0.0, 100.0), pct.iter().map(|p| p.round().clamp(0.0, 100.0) as u8).collect())
}

fn read_counters() -> Option<Vec<Counters>> {
    let mut buf = vec![RawPerf::default(); MAX_CORES];
    let mut ret = 0u32;
    // SAFETY: `buf` is a writable array of `MAX_CORES` repr(C) structs and the length matches.
    let status = unsafe {
        NtQuerySystemInformation(
            SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION,
            buf.as_mut_ptr().cast(),
            (MAX_CORES * size_of::<RawPerf>()) as u32,
            &mut ret,
        )
    };
    if status < 0 {
        return None;
    }
    let n = ret as usize / size_of::<RawPerf>();
    Some(
        buf[..n]
            .iter()
            .map(|r| Counters { idle: r.idle as u64, kernel: r.kernel as u64, user: r.user as u64 })
            .collect(),
    )
}

/// `(total_mb, used_mb)`.
fn read_ram() -> Option<(u32, u32)> {
    // SAFETY: an all-zero MEMORYSTATUSEX is valid; `dwLength` is set as the API requires.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: `m` is a valid, correctly sized out-parameter.
    if unsafe { GlobalMemoryStatusEx(&mut m) } == 0 {
        return None;
    }
    let total = (m.ullTotalPhys / MB) as u32;
    Some((total, total.saturating_sub((m.ullAvailPhys / MB) as u32)))
}

#[repr(C)]
#[derive(Default)]
struct NvmlMemory {
    total: u64,
    free: u64,
    used: u64,
}

#[repr(C)]
#[derive(Default)]
struct NvmlUtilization {
    gpu: u32,
    memory: u32,
}

type InitFn = unsafe extern "C" fn() -> i32;
type ShutdownFn = unsafe extern "C" fn() -> i32;
type HandleFn = unsafe extern "C" fn(u32, *mut *mut c_void) -> i32;
type NameFn = unsafe extern "C" fn(*mut c_void, *mut c_char, u32) -> i32;
type MemoryFn = unsafe extern "C" fn(*mut c_void, *mut NvmlMemory) -> i32;
type UtilFn = unsafe extern "C" fn(*mut c_void, *mut NvmlUtilization) -> i32;
type TempFn = unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> i32;
type PowerFn = unsafe extern "C" fn(*mut c_void, *mut u32) -> i32;

/// A loaded and initialised NVML with device 0.
struct Nvml {
    lib: *mut c_void,
    device: *mut c_void,
    shutdown: ShutdownFn,
    memory: MemoryFn,
    util: UtilFn,
    temp: TempFn,
    power: PowerFn,
    info: GpuInfo,
}

/// Injected failures. All fields stay at their defaults in release builds.
#[derive(Debug, Default, PartialEq, Eq)]
struct Faults {
    nvml_dll: bool,
    nvml_init: bool,
    gpu_at: Option<u32>,
    ram_at: Option<u32>,
    cpu_at: Option<u32>,
}

/// Reads fail for this many consecutive iterations starting at `@N`.
const FAULT_SPAN: u32 = 3;

impl Faults {
    #[cfg(debug_assertions)]
    fn from_env() -> Faults {
        Faults::parse(&std::env::var("EREBUS_FAULT").unwrap_or_default())
    }

    #[cfg(not(debug_assertions))]
    fn from_env() -> Faults {
        Faults::default()
    }

    #[cfg(any(debug_assertions, test))]
    fn parse(spec: &str) -> Faults {
        let mut f = Faults::default();
        for tok in spec.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            let at = |name: &str| tok.strip_prefix(name)?.strip_prefix('@')?.parse::<u32>().ok();
            match tok {
                "nvml-dll" => f.nvml_dll = true,
                "nvml-init" => f.nvml_init = true,
                _ if at("gpu").is_some() => f.gpu_at = at("gpu"),
                _ if at("ram").is_some() => f.ram_at = at("ram"),
                _ if at("cpu").is_some() => f.cpu_at = at("cpu"),
                _ => log_warn!("unknown EREBUS_FAULT token {tok:?}"),
            }
        }
        f
    }

    /// True when `at` covers loop iteration `n`.
    fn hits(at: Option<u32>, n: u32) -> bool {
        at.is_some_and(|a| (a..a + FAULT_SPAN).contains(&n))
    }
}

/// Warns on the first failure of a read, and reports its recovery once.
struct ReadWatch {
    what: &'static str,
    warned: bool,
}

impl ReadWatch {
    fn new(what: &'static str) -> ReadWatch {
        ReadWatch { what, warned: false }
    }

    fn failed(&mut self, effect: &str) {
        if !std::mem::replace(&mut self.warned, true) {
            log_warn!("{} read failed; {effect}", self.what);
        }
    }

    fn ok(&mut self) {
        if std::mem::replace(&mut self.warned, false) {
            log_info!("{} read recovered", self.what);
        }
    }
}

impl Nvml {
    fn load(faults: &Faults) -> Result<Nvml, String> {
        if std::env::var("EREBUS_NVML").is_ok_and(|v| v.trim() == "0") {
            return Err("disabled by EREBUS_NVML=0".into());
        }
        if faults.nvml_dll {
            return Err("nvml.dll not found (injected)".into());
        }
        if faults.nvml_init {
            return Err("nvmlInit_v2 failed (injected)".into());
        }
        let name: Vec<u16> = "nvml.dll\0".encode_utf16().collect();
        // SAFETY: `name` is a NUL-terminated UTF-16 string.
        let lib = unsafe { LoadLibraryW(name.as_ptr()) };
        if lib.is_null() {
            return Err("nvml.dll not found".into());
        }
        Nvml::bind(lib).inspect_err(|_| {
            // SAFETY: `lib` was returned by `LoadLibraryW` above and nothing else uses it.
            unsafe { FreeLibrary(lib) };
        })
    }

    fn bind(lib: *mut c_void) -> Result<Nvml, String> {
        macro_rules! sym {
            ($name:literal, $ty:ty) => {{
                // SAFETY: `lib` is a loaded module and the name is NUL-terminated.
                match unsafe { GetProcAddress(lib, concat!($name, "\0").as_ptr()) } {
                    // SAFETY: the NVML export has the signature `$ty`.
                    Some(f) => unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, $ty>(f) },
                    None => return Err(format!("{} missing", $name)),
                }
            }};
        }
        let init: InitFn = sym!("nvmlInit_v2", InitFn);
        let shutdown: ShutdownFn = sym!("nvmlShutdown", ShutdownFn);
        let handle: HandleFn = sym!("nvmlDeviceGetHandleByIndex_v2", HandleFn);
        let name_fn: NameFn = sym!("nvmlDeviceGetName", NameFn);
        let memory: MemoryFn = sym!("nvmlDeviceGetMemoryInfo", MemoryFn);
        let util: UtilFn = sym!("nvmlDeviceGetUtilizationRates", UtilFn);
        let temp: TempFn = sym!("nvmlDeviceGetTemperature", TempFn);
        let power: PowerFn = sym!("nvmlDeviceGetPowerUsage", PowerFn);

        // SAFETY: each call matches the NVML C signature; buffers outlive the calls.
        unsafe {
            let rc = init();
            if rc != 0 {
                return Err(format!("nvmlInit_v2 returned {rc}"));
            }
            let mut device = std::ptr::null_mut();
            let mut buf = [0 as c_char; 96];
            let mut mem = NvmlMemory::default();
            let rc = handle(0, &mut device);
            let ok = rc == 0
                && name_fn(device, buf.as_mut_ptr(), buf.len() as u32) == 0
                && memory(device, &mut mem) == 0;
            if !ok {
                shutdown();
                return Err(format!("no readable device 0 (handle rc {rc})"));
            }
            let name = CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
            let info = GpuInfo { name, vram_total_mb: (mem.total / MB) as u32 };
            Ok(Nvml { lib, device, shutdown, memory, util, temp, power, info })
        }
    }

    fn read(&self) -> Option<GpuSample> {
        let mut mem = NvmlMemory::default();
        let mut util = NvmlUtilization::default();
        let (mut temp, mut mw) = (0u32, 0u32);
        // SAFETY: the device handle is valid while `self` lives and the out-pointers are locals.
        let ok = unsafe {
            (self.memory)(self.device, &mut mem) == 0
                && (self.util)(self.device, &mut util) == 0
                && (self.temp)(self.device, 0, &mut temp) == 0
                && (self.power)(self.device, &mut mw) == 0
        };
        ok.then(|| GpuSample {
            util: util.gpu.min(100) as u8,
            vram_used_mb: (mem.used / MB) as u32,
            temp_c: temp.min(255) as u8,
            power_w: (mw / 1000).min(u16::MAX as u32) as u16,
        })
    }
}

impl Drop for Nvml {
    fn drop(&mut self) {
        // SAFETY: NVML was initialised in `bind`; the library is freed after shutdown.
        unsafe {
            (self.shutdown)();
            FreeLibrary(self.lib);
        }
    }
}

fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Time until the next whole wall-clock second.
fn until_next_second(now_ms: u64) -> Duration {
    Duration::from_millis(1000 - now_ms % 1000)
}

pub fn spawn(hub: UnboundedSender<Event>) {
    let spawned = thread::Builder::new().name("sysmon".into()).spawn(move || run(hub));
    if let Err(e) = spawned {
        log_warn!("cannot start vitals sampler: {e}");
    }
}

fn run(hub: UnboundedSender<Event>) {
    let (Some(mut prev), Some((ram_total_mb, _))) = (read_counters(), read_ram()) else {
        log_warn!("vitals unavailable: initial CPU or RAM read failed");
        return;
    };
    let faults = Faults::from_env();
    let nvml = match Nvml::load(&faults) {
        Ok(n) => Some(n),
        Err(why) => {
            log_info!("no GPU vitals: {why}");
            None
        }
    };
    let hw = Hardware { ram_total_mb, cores: prev.len() as u16, gpu: nvml.as_ref().map(|n| n.info.clone()) };
    if hub.send(Event::Hardware(hw)).is_err() {
        return;
    }

    let (mut cpu_watch, mut ram_watch, mut gpu_watch) =
        (ReadWatch::new("CPU"), ReadWatch::new("RAM"), ReadWatch::new("GPU"));
    let mut n = 0u32;
    loop {
        thread::sleep(until_next_second(unix_ms()));
        n += 1;
        let cur = read_counters().filter(|_| !Faults::hits(faults.cpu_at, n));
        let Some(cur) = cur else {
            cpu_watch.failed("samples are skipped");
            continue;
        };
        cpu_watch.ok();
        let (cpu, cores) = cpu_percent(&prev, &cur);
        prev = cur;
        let ram = read_ram().filter(|_| !Faults::hits(faults.ram_at, n));
        let Some((_, ram_used_mb)) = ram else {
            ram_watch.failed("samples are skipped");
            continue;
        };
        ram_watch.ok();
        let gpu = nvml.as_ref().and_then(|nv| {
            let g = nv.read().filter(|_| !Faults::hits(faults.gpu_at, n));
            match g {
                Some(_) => gpu_watch.ok(),
                None => gpu_watch.failed("affected samples carry no GPU"),
            }
            g
        });
        if hub.send(Event::Vitals(Sample { unix_ms: unix_ms(), ram_used_mb, cpu, cores, gpu })).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(idle: u64, kernel: u64, user: u64) -> Counters {
        Counters { idle, kernel, user }
    }

    #[test]
    fn half_busy() {
        // 100 ticks total, 50 idle.
        assert_eq!(core_percent(c(0, 0, 0), c(50, 80, 20)), 50.0);
    }

    #[test]
    fn fully_idle_and_fully_busy() {
        assert_eq!(core_percent(c(0, 0, 0), c(100, 100, 0)), 0.0);
        assert_eq!(core_percent(c(0, 0, 0), c(0, 60, 40)), 100.0);
    }

    #[test]
    fn equal_counters_give_zero_not_nan() {
        let x = c(10, 20, 30);
        let p = core_percent(x, x);
        assert_eq!(p, 0.0);
        assert!(!p.is_nan());
    }

    #[test]
    fn idle_above_total_gives_zero() {
        assert_eq!(core_percent(c(0, 0, 0), c(200, 100, 0)), 0.0);
    }

    #[test]
    fn backwards_counters_give_zero() {
        assert_eq!(core_percent(c(100, 200, 100), c(0, 0, 0)), 0.0);
    }

    #[test]
    fn mean_and_rounding() {
        let prev = [c(0, 0, 0), c(0, 0, 0), c(0, 0, 0)];
        let cur = [c(100, 100, 0), c(0, 100, 0), c(50, 100, 0)];
        let (cpu, cores) = cpu_percent(&prev, &cur);
        assert_eq!(cores, vec![0, 100, 50]);
        assert_eq!(cpu, 50.0);
    }

    #[test]
    fn empty_and_mismatched_inputs() {
        assert_eq!(cpu_percent(&[], &[]), (0.0, vec![]));
        let (_, cores) = cpu_percent(&[c(0, 0, 0); 4], &[c(0, 10, 0); 2]);
        assert_eq!(cores.len(), 2);
    }

    #[test]
    fn next_second_wait_is_within_a_second() {
        assert_eq!(until_next_second(5_000), Duration::from_millis(1000));
        assert_eq!(until_next_second(5_250), Duration::from_millis(750));
        assert_eq!(until_next_second(5_999), Duration::from_millis(1));
    }

    #[test]
    fn fault_tokens_parse() {
        let f = Faults::parse("nvml-dll, gpu@2,ram@5,cpu@9,bogus,gpu@x");
        assert_eq!(
            f,
            Faults { nvml_dll: true, nvml_init: false, gpu_at: Some(2), ram_at: Some(5), cpu_at: Some(9) }
        );
        assert_eq!(Faults::parse(""), Faults::default());
        assert!(Faults::parse("nvml-init").nvml_init);
    }

    #[test]
    fn fault_window_covers_three_iterations() {
        let at = Some(2);
        let hit: Vec<u32> = (1..=6).filter(|n| Faults::hits(at, *n)).collect();
        assert_eq!(hit, vec![2, 3, 4]);
        assert!(!Faults::hits(None, 2));
    }

    #[test]
    fn read_watch_warns_once_and_rearms() {
        let mut w = ReadWatch::new("X");
        w.failed("e");
        assert!(w.warned);
        w.failed("e");
        w.ok();
        assert!(!w.warned);
    }

    #[test]
    fn live_reads_are_sane() {
        let cores = read_counters().expect("cpu counters");
        assert!(!cores.is_empty());
        let (total, used) = read_ram().expect("ram");
        assert!(total > 0 && used <= total);
    }
}
