pub fn get_memory_usage() -> usize {
  use windows::Win32::System::ProcessStatus::*;
  use windows::Win32::System::Threading::GetCurrentProcess;

  unsafe {
    let process = GetCurrentProcess();
    let mut pmc = PROCESS_MEMORY_COUNTERS {
      cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
      ..Default::default()
    };

    if GetProcessMemoryInfo(process, &mut pmc, pmc.cb).is_ok() {
      return pmc.WorkingSetSize;
    }
  }

  0
}
