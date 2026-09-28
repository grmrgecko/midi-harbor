//! Apple's own MIDI setup, read for a one-time import.
//!
//! Read only, through CoreMIDI's queries: nothing here writes Apple's configuration, which
//! research R-002 rules out. The IAC Driver's buses are the entities of its device, and each
//! network session is an entity of the network driver's device.

/// What Apple's MIDI setup holds that Midi Harbor can take over.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppleSetup {
    /// Whether the IAC Driver is switched on, so its buses are visible to applications now.
    pub iac_online: bool,
    /// The IAC Driver's bus names.
    pub buses: Vec<String>,
    /// The network sessions configured in Audio MIDI Setup.
    pub sessions: Vec<String>,
}

/// Reads Apple's MIDI setup, or nothing on a platform that has none.
#[cfg(target_os = "macos")]
pub fn read() -> Option<AppleSetup> {
    macos::read()
}

/// Reads Apple's MIDI setup, or nothing on a platform that has none.
#[cfg(not(target_os = "macos"))]
pub fn read() -> Option<AppleSetup> {
    None
}

#[cfg(target_os = "macos")]
mod macos {
    use super::AppleSetup;
    use core_foundation_sys::base::CFRelease;
    use core_foundation_sys::string::{
        CFStringGetCString, CFStringGetLength, CFStringGetMaximumSizeForEncoding, CFStringRef,
        kCFStringEncodingUTF8,
    };
    use coremidi_sys::{
        MIDIDeviceGetEntity, MIDIDeviceGetNumberOfEntities, MIDIGetDevice, MIDIGetNumberOfDevices,
        MIDIObjectGetIntegerProperty, MIDIObjectGetStringProperty, MIDIObjectRef,
        kMIDIPropertyDriverOwner, kMIDIPropertyName, kMIDIPropertyOffline,
    };

    /// The driver that owns the IAC buses.
    const IAC_DRIVER: &str = "com.apple.AppleMIDIIACDriver";

    /// The driver that owns Apple's network sessions.
    const NETWORK_DRIVER: &str = "com.apple.AppleMIDIRTPDriver";

    pub(super) fn read() -> Option<AppleSetup> {
        let mut setup = AppleSetup::default();

        // SAFETY: takes no arguments and only reads CoreMIDI's device count.
        let devices = unsafe { MIDIGetNumberOfDevices() };
        for index in 0..devices {
            // SAFETY: `index` is below the count CoreMIDI just reported.
            let device = unsafe { MIDIGetDevice(index) };
            if device == 0 {
                continue;
            }
            let driver = string_property(device, Property::Driver);
            let is_iac = driver.as_deref() == Some(IAC_DRIVER);
            let is_network = driver.as_deref() == Some(NETWORK_DRIVER);
            if !is_iac && !is_network {
                continue;
            }

            let names = entity_names(device);
            if is_iac {
                setup.iac_online = !offline(device);
                setup.buses = names;
            } else {
                setup.sessions = names;
            }
        }
        Some(setup)
    }

    /// Which string property to read.
    enum Property {
        Name,
        Driver,
    }

    /// Returns the names of a device's entities.
    fn entity_names(device: MIDIObjectRef) -> Vec<String> {
        // SAFETY: `device` came from `MIDIGetDevice` in this pass.
        let count = unsafe { MIDIDeviceGetNumberOfEntities(device) };
        (0..count)
            .filter_map(|index| {
                // SAFETY: `index` is below the count CoreMIDI just reported for this device.
                let entity = unsafe { MIDIDeviceGetEntity(device, index) };
                (entity != 0)
                    .then(|| string_property(entity, Property::Name))
                    .flatten()
            })
            .filter(|name| !name.trim().is_empty())
            .collect()
    }

    /// Reports whether a device is switched off.
    fn offline(device: MIDIObjectRef) -> bool {
        let mut value = 0;
        // SAFETY: `device` is a live object reference and the out-pointer is to an initialised
        // local that outlives the call.
        let status =
            unsafe { MIDIObjectGetIntegerProperty(device, kMIDIPropertyOffline, &raw mut value) };
        status == 0 && value != 0
    }

    /// Reads one string property, copying it out of CoreFoundation.
    fn string_property(object: MIDIObjectRef, property: Property) -> Option<String> {
        let key = match property {
            // SAFETY: CoreMIDI's property keys are constants that live for the whole process.
            Property::Name => unsafe { kMIDIPropertyName },
            // SAFETY: as above.
            Property::Driver => unsafe { kMIDIPropertyDriverOwner },
        };
        let mut value: CFStringRef = std::ptr::null();
        // SAFETY: `object` is a live object reference and the out-pointer is to an initialised
        // local. On success CoreMIDI hands over a retained string, released below.
        let status = unsafe { MIDIObjectGetStringProperty(object, key, &raw mut value) };
        if status != 0 || value.is_null() {
            return None;
        }
        let text = copy_string(value);
        // SAFETY: `value` is the retained string CoreMIDI returned, released exactly once.
        unsafe { CFRelease(value.cast()) };
        text
    }

    /// Copies a CoreFoundation string into an owned one.
    fn copy_string(value: CFStringRef) -> Option<String> {
        // SAFETY: `value` is a live string for the duration of these calls.
        let length = unsafe { CFStringGetLength(value) };
        // SAFETY: as above; this only computes a size.
        let capacity = unsafe { CFStringGetMaximumSizeForEncoding(length, kCFStringEncodingUTF8) }
            .checked_add(1)?;
        let mut buffer = vec![0u8; usize::try_from(capacity).ok()?];
        // SAFETY: the buffer holds `capacity` bytes, which is what CoreFoundation is told it may
        // write, including the terminating zero.
        let copied = unsafe {
            CFStringGetCString(
                value,
                buffer.as_mut_ptr().cast(),
                capacity,
                kCFStringEncodingUTF8,
            )
        };
        if copied == 0 {
            return None;
        }
        let end = buffer
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(buffer.len());
        buffer.truncate(end);
        String::from_utf8(buffer).ok()
    }
}
