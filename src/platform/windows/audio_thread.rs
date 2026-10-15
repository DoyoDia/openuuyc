//! COM ownership for dedicated WASAPI workers without a UI message loop.
use std::{marker::PhantomData, rc::Rc};
use windows::{
    Win32::System::Com::{
        APTTYPE, APTTYPEQUALIFIER, COINIT_MULTITHREADED, CoGetApartmentType, CoInitializeEx,
        CoUninitialize,
    },
    core::Result,
};

struct Apartment(PhantomData<Rc<()>>);

impl Apartment {
    fn new() -> Result<Self> {
        // A mode conflict is an error: this entry belongs only on a fresh worker,
        // never on the STA UI thread or an arbitrary shared executor thread.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        Ok(Self(PhantomData))
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

thread_local! {
    // Initialize before CPAL's thread-local COM/enumerator caches so those caches
    // are destroyed first. A stack guard would end before CPAL's TLS cleanup.
    static APARTMENT: Result<Apartment> = Apartment::new();
}

pub(crate) fn initialize() -> Result<()> {
    APARTMENT.with(|apartment| apartment.as_ref().map(|_| ()).map_err(Clone::clone))
}

pub(crate) fn description() -> String {
    let mut kind = APTTYPE::default();
    let mut qualifier = APTTYPEQUALIFIER::default();
    match unsafe { CoGetApartmentType(&mut kind, &mut qualifier) } {
        Ok(()) => format!("{kind:?}/{qualifier:?}"),
        Err(error) => format!("unavailable: {error}"),
    }
}
