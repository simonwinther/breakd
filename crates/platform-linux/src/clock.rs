use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use breakd_core::ClockSample;
use nix::{sys::time::TimeValLike, time::ClockId};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClockError {
    #[error("kernel clock read failed: {0}")]
    Kernel(#[from] nix::errno::Errno),
    #[error("system clock is before the Unix epoch")]
    WallClock,
    #[error("failed to read boot ID: {0}")]
    BootId(#[from] std::io::Error),
}

#[derive(Debug, Clone, Default)]
pub struct LinuxClock;

impl LinuxClock {
    pub fn sample(&self) -> Result<ClockSample, ClockError> {
        let monotonic_ms = ClockId::CLOCK_MONOTONIC
            .now()?
            .num_milliseconds()
            .try_into()
            .unwrap_or(0);
        let boottime_ms = ClockId::CLOCK_BOOTTIME
            .now()?
            .num_milliseconds()
            .try_into()
            .unwrap_or(0);
        let wall_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ClockError::WallClock)?
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        Ok(ClockSample {
            monotonic_ms,
            boottime_ms,
            wall_unix_ms,
        })
    }

    pub fn boot_id(&self) -> Result<String, ClockError> {
        Ok(fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .to_owned())
    }
}
