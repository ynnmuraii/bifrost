use crate::Error;
use std::marker::PhantomData;
use windows::{
    core::w,
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::{
            Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED},
            Threading::{AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW},
        },
    },
};

pub(crate) struct Apartment(PhantomData<*mut ()>);
impl Apartment {
    pub(crate) fn mta() -> Result<Self, Error> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        }
        Ok(Self(PhantomData))
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

pub(crate) struct Event(pub HANDLE);
impl Drop for Event {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

pub(crate) struct ProAudio(Option<HANDLE>);
impl ProAudio {
    pub(crate) fn enter() -> Self {
        let mut index = 0;
        Self(unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut index) }.ok())
    }
}
impl Drop for ProAudio {
    fn drop(&mut self) {
        if let Some(handle) = self.0 {
            let _ = unsafe { AvRevertMmThreadCharacteristics(handle) };
        }
    }
}
