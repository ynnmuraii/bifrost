//! Shared render negotiation, performed before entering the real-time worker loop.
//! The PCM contract stays 48 kHz f32 stereo. IAudioClient3 cannot auto-convert;
//! unsupported formats use a fresh legacy shared client with engine conversion.
use crate::{stream::SAMPLE_RATE, Error};
use wasapi::{Device, Direction, SampleType, StreamMode, WaveFormat};
use windows::{
    core::{Interface, HRESULT},
    Win32::{
        Foundation::{E_NOINTERFACE, E_NOTIMPL},
        Media::Audio::*,
        System::Com::CoTaskMemFree,
    },
};

/// Production floor: at least 5 ms, rounded up to a supported fundamental multiple.
pub const SAFE_PERIOD_FRAMES: u32 = SAMPLE_RATE / 200;
pub const MAX_QUEUED_PERIODS: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderMode {
    LowLatencyShared,
    LegacyShared,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderPeriodSource {
    RequestedSharedPeriod,
    CurrentSharedEngine,
    /// Used only if IAudioClient3/current-period querying is unsupported.
    LegacyDefaultEstimate,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackReason {
    InterfaceUnavailable,
    UnsupportedFormat,
    PeriodQueryFailed,
    NoSupportedPeriod,
    UnsupportedPeriod,
    PeriodicityLocked,
    FormatLocked,
    InitializationFailed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderFallback {
    pub reason: FallbackReason,
    /// Original failed operation's HRESULT; absent for an invalid/unsuitable period range.
    pub hresult: Option<i32>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderConfiguration {
    pub mode: RenderMode,
    pub fallback: Option<RenderFallback>,
    /// Period duration in whole 48 kHz client frames. Converted legacy periods round
    /// down so the two-period queue bound never rounds above the engine duration.
    pub period_frames: u32,
    pub period_source: RenderPeriodSource,
    /// WASAPI allocation, not the amount of PCM queued by this adapter.
    pub capacity_frames: u32,
    /// min(capacity, 2 * period). Padding is subtracted before requesting a buffer.
    pub queue_limit_frames: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnginePeriods {
    pub default: u32,
    pub fundamental: u32,
    pub minimum: u32,
    pub maximum: u32,
}
impl EnginePeriods {
    /// Smallest supported fundamental multiple at or above the requested floor.
    pub fn select(self, floor_frames: u32) -> Option<u32> {
        if self.fundamental == 0 || self.minimum == 0 || self.minimum > self.maximum {
            return None;
        }
        let floor = self.minimum.max(floor_frames);
        let period = floor
            .div_ceil(self.fundamental)
            .checked_mul(self.fundamental)?;
        (period <= self.maximum).then_some(period)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackStage {
    Interface,
    PeriodQuery,
    Initialization,
}
/// Pure classification preserves driver-specific HRESULTs without formatting on the RT thread.
pub fn classify_fallback(stage: FallbackStage, error: HRESULT) -> RenderFallback {
    let reason = if stage == FallbackStage::Interface {
        FallbackReason::InterfaceUnavailable
    } else if error == AUDCLNT_E_UNSUPPORTED_FORMAT {
        FallbackReason::UnsupportedFormat
    } else if error == AUDCLNT_E_INVALID_DEVICE_PERIOD {
        FallbackReason::UnsupportedPeriod
    } else if error == AUDCLNT_E_ENGINE_PERIODICITY_LOCKED {
        FallbackReason::PeriodicityLocked
    } else if error == AUDCLNT_E_ENGINE_FORMAT_LOCKED {
        FallbackReason::FormatLocked
    } else if stage == FallbackStage::PeriodQuery {
        FallbackReason::PeriodQueryFailed
    } else {
        FallbackReason::InitializationFailed
    };
    RenderFallback {
        reason,
        hresult: Some(error.0),
    }
}
fn format() -> WaveFormat {
    WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE as usize, 2, None)
}
fn try_low_latency(raw: &IAudioClient, floor_frames: u32) -> Result<u32, RenderFallback> {
    let client: IAudioClient3 = raw
        .cast()
        .map_err(|e| classify_fallback(FallbackStage::Interface, e.code()))?;
    let format = format();
    let mut periods = EnginePeriods {
        default: 0,
        fundamental: 0,
        minimum: 0,
        maximum: 0,
    };
    unsafe {
        client.GetSharedModeEnginePeriod(
            format.as_waveformatex_ref(),
            &mut periods.default,
            &mut periods.fundamental,
            &mut periods.minimum,
            &mut periods.maximum,
        )
    }
    .map_err(|e| classify_fallback(FallbackStage::PeriodQuery, e.code()))?;
    let period = periods.select(floor_frames).ok_or(RenderFallback {
        reason: FallbackReason::NoSupportedPeriod,
        hresult: None,
    })?;
    // No AUTOCONVERTPCM here: IAudioClient3 requires support for this exact PCM contract.
    unsafe {
        client.InitializeSharedAudioStream(
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            period,
            format.as_waveformatex_ref(),
            None,
        )
    }
    .map_err(|e| classify_fallback(FallbackStage::Initialization, e.code()))?;
    Ok(period)
}
/// Initialize only; never starts playback or changes endpoint settings.
/// `floor_frames` is diagnostic policy; production callers use SAFE_PERIOD_FRAMES.
/// A failed low-latency attempt is discarded before legacy initialization.
pub fn initialize_shared_render(
    device: &Device,
    floor_frames: u32,
) -> Result<(IAudioClient, RenderConfiguration), Error> {
    let client = device.get_iaudioclient()?;
    let raw = client.as_iaudioclient();
    match try_low_latency(raw, floor_frames) {
        Ok(period) => {
            let config = configuration(
                raw,
                RenderMode::LowLatencyShared,
                period,
                RenderPeriodSource::RequestedSharedPeriod,
                None,
            )?;
            Ok((raw.clone(), config))
        }
        Err(fallback) => {
            drop(client);
            initialize_legacy_render(device, Some(fallback))
        }
    }
}
/// The original shared/event/auto-conversion path, also exposed for diagnostic comparisons.
pub fn initialize_legacy_render(
    device: &Device,
    fallback: Option<RenderFallback>,
) -> Result<(IAudioClient, RenderConfiguration), Error> {
    let mut client = device.get_iaudioclient()?;
    client.initialize_client(
        &format(),
        &Direction::Render,
        &StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: 0,
        },
    )?;
    let raw = client.as_iaudioclient();
    // A legacy stream can join an engine already running at another client's shorter period.
    let (period, source) = resolve_legacy_period(current_engine_period(raw)?, || {
        client
            .get_device_period()
            .map(|period| period.0)
            .map_err(Error::from)
    })?;
    let config = configuration(raw, RenderMode::LegacyShared, period, source, fallback)?;
    Ok((raw.clone(), config))
}
/// Conservatively convert duration to client frames; only legacy conversion can be fractional.
fn period_in_client_frames(frames: u32, engine_rate: u32) -> Option<u32> {
    if frames == 0 || engine_rate == 0 {
        return None;
    }
    u32::try_from(u64::from(frames) * u64::from(SAMPLE_RATE) / u64::from(engine_rate))
        .ok()
        .filter(|&frames| frames != 0)
}
fn resolve_legacy_period(
    current: Option<u32>,
    default_period: impl FnOnce() -> Result<i64, Error>,
) -> Result<(u32, RenderPeriodSource), Error> {
    if let Some(period) = current {
        return Ok((period, RenderPeriodSource::CurrentSharedEngine));
    }
    let period = u64::try_from(default_period()?)
        .ok()
        .and_then(|hns| hns.checked_mul(u64::from(SAMPLE_RATE)))
        .map(|frames| frames / 10_000_000)
        .and_then(|frames| u32::try_from(frames).ok())
        .filter(|&frames| frames != 0)
        .ok_or(Error::Unsupported("invalid legacy render period"))?;
    Ok((period, RenderPeriodSource::LegacyDefaultEstimate))
}
fn current_engine_period(raw: &IAudioClient) -> Result<Option<u32>, Error> {
    let client: IAudioClient3 = match raw.cast() {
        Ok(client) => client,
        Err(error) if period_query_unsupported(error.code()) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut engine_format: *mut WAVEFORMATEX = std::ptr::null_mut();
    let mut engine_frames = 0;
    let result =
        unsafe { client.GetCurrentSharedModeEnginePeriod(&mut engine_format, &mut engine_frames) };
    let engine_rate = if result.is_ok() && !engine_format.is_null() {
        Some(unsafe { (*engine_format).nSamplesPerSec })
    } else {
        None
    };
    // Free COM-owned format memory on every path, including a pointer returned on error.
    if !engine_format.is_null() {
        unsafe { CoTaskMemFree(Some(engine_format.cast())) };
    }
    if let Err(error) = result {
        return if period_query_unsupported(error.code()) {
            Ok(None)
        } else {
            Err(error.into())
        };
    }
    let frames = engine_rate
        .and_then(|rate| period_in_client_frames(engine_frames, rate))
        .ok_or(Error::Unsupported("invalid current shared engine period"))?;
    Ok(Some(frames))
}
fn period_query_unsupported(error: HRESULT) -> bool {
    error == E_NOINTERFACE || error == E_NOTIMPL || error == AUDCLNT_E_UNSUPPORTED_FORMAT
}
fn configuration(
    raw: &IAudioClient,
    mode: RenderMode,
    period_frames: u32,
    period_source: RenderPeriodSource,
    fallback: Option<RenderFallback>,
) -> Result<RenderConfiguration, Error> {
    let capacity_frames = unsafe { raw.GetBufferSize()? };
    if capacity_frames == 0 || capacity_frames > SAMPLE_RATE {
        return Err(Error::Unsupported("render buffer outside one-second bound"));
    }
    Ok(RenderConfiguration {
        mode,
        fallback,
        period_frames,
        period_source,
        capacity_frames,
        queue_limit_frames: capacity_frames.min(period_frames.saturating_mul(MAX_QUEUED_PERIODS)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn current_engine_period_overrides_default_without_querying_it() {
        let (period, source) = resolve_legacy_period(Some(240), || {
            panic!("default must not override the running engine")
        })
        .unwrap();
        assert_eq!(period, 240);
        assert_eq!(source, RenderPeriodSource::CurrentSharedEngine);
        let (period, source) = resolve_legacy_period(None, || Ok(100_000)).unwrap();
        assert_eq!(period, 480);
        assert_eq!(source, RenderPeriodSource::LegacyDefaultEstimate);
        assert!(resolve_legacy_period(None, || Ok(0)).is_err());
        assert!(resolve_legacy_period(None, || Ok(-1)).is_err());
        assert!(resolve_legacy_period(None, || Ok(i64::MAX)).is_err());
        assert!(resolve_legacy_period(None, || Err(Error::WorkerStopped)).is_err());
    }
    #[test]
    fn engine_duration_conversion_cannot_exceed_two_actual_periods() {
        assert_eq!(period_in_client_frames(240, 48_000), Some(240));
        assert_eq!(period_in_client_frames(480, 96_000), Some(240));
        assert_eq!(period_in_client_frames(441, 44_100), Some(480));
        assert_eq!(period_in_client_frames(221, 44_100), Some(240));
        for rate in [44_100, 48_000, 96_000, 192_000] {
            for frames in [128, 221, 240, 441, 480] {
                let client = period_in_client_frames(frames, rate).unwrap();
                assert!(
                    u64::from(client) * 2 * u64::from(rate)
                        <= u64::from(frames) * 2 * u64::from(SAMPLE_RATE)
                );
            }
        }
        assert_eq!(period_in_client_frames(0, 48_000), None);
        assert_eq!(period_in_client_frames(240, 0), None);
        assert_eq!(period_in_client_frames(1, 192_000), None);
        assert_eq!(period_in_client_frames(u32::MAX, 1), None);
    }
    #[test]
    fn only_unsupported_current_queries_allow_default_estimation() {
        for error in [E_NOINTERFACE, E_NOTIMPL, AUDCLNT_E_UNSUPPORTED_FORMAT] {
            assert!(period_query_unsupported(error));
        }
        for error in [
            AUDCLNT_E_DEVICE_INVALIDATED,
            AUDCLNT_E_SERVICE_NOT_RUNNING,
            AUDCLNT_E_NOT_INITIALIZED,
            HRESULT(0x80070057_u32 as i32),
        ] {
            assert!(!period_query_unsupported(error));
        }
    }
    #[test]
    fn selects_smallest_supported_multiple_above_safe_floor() {
        let periods = EnginePeriods {
            default: 480,
            fundamental: 48,
            minimum: 48,
            maximum: 960,
        };
        assert_eq!(periods.select(SAFE_PERIOD_FRAMES), Some(240));
        assert_eq!(periods.select(241), Some(288));
        assert_eq!(periods.select(0), Some(48));
        assert_eq!(periods.select(480), Some(480));
        assert_eq!(periods.select(961), None);
        assert_eq!(
            EnginePeriods {
                minimum: 250,
                ..periods
            }
            .select(240),
            Some(288)
        );
    }
    #[test]
    fn rejects_invalid_ranges_and_overflow() {
        let periods = EnginePeriods {
            default: 480,
            fundamental: 0,
            minimum: 48,
            maximum: 960,
        };
        assert_eq!(periods.select(240), None);
        assert_eq!(
            EnginePeriods {
                fundamental: 48,
                minimum: 0,
                ..periods
            }
            .select(240),
            None
        );
        assert_eq!(
            EnginePeriods {
                fundamental: 48,
                minimum: 961,
                ..periods
            }
            .select(240),
            None
        );
        assert_eq!(
            EnginePeriods {
                fundamental: 2,
                maximum: u32::MAX,
                ..periods
            }
            .select(u32::MAX),
            None
        );
        assert_eq!(
            EnginePeriods {
                fundamental: 48,
                maximum: 239,
                ..periods
            }
            .select(240),
            None
        );
    }
    #[test]
    fn classifies_fallback_and_preserves_hresult() {
        for (stage, hr, expected) in [
            (
                FallbackStage::Interface,
                HRESULT(0x80004002_u32 as i32),
                FallbackReason::InterfaceUnavailable,
            ),
            (
                FallbackStage::PeriodQuery,
                AUDCLNT_E_UNSUPPORTED_FORMAT,
                FallbackReason::UnsupportedFormat,
            ),
            (
                FallbackStage::Initialization,
                AUDCLNT_E_UNSUPPORTED_FORMAT,
                FallbackReason::UnsupportedFormat,
            ),
            (
                FallbackStage::Initialization,
                AUDCLNT_E_INVALID_DEVICE_PERIOD,
                FallbackReason::UnsupportedPeriod,
            ),
            (
                FallbackStage::Initialization,
                AUDCLNT_E_ENGINE_PERIODICITY_LOCKED,
                FallbackReason::PeriodicityLocked,
            ),
            (
                FallbackStage::Initialization,
                AUDCLNT_E_ENGINE_FORMAT_LOCKED,
                FallbackReason::FormatLocked,
            ),
            (
                FallbackStage::PeriodQuery,
                HRESULT(0x80070057_u32 as i32),
                FallbackReason::PeriodQueryFailed,
            ),
            (
                FallbackStage::Initialization,
                HRESULT(0x80070057_u32 as i32),
                FallbackReason::InitializationFailed,
            ),
        ] {
            assert_eq!(
                classify_fallback(stage, hr),
                RenderFallback {
                    reason: expected,
                    hresult: Some(hr.0)
                }
            );
        }
    }
}
