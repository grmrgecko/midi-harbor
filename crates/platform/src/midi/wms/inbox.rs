//! Virtual ports through the API Windows carries, `Windows.Devices.Midi2`.

use super::inbox_bindings::Windows::Devices::Midi2::Enumeration::{
    MidiDeclaredEndpointInfo, MidiFunctionBlock, MidiFunctionBlockDirection,
    MidiFunctionBlockRepresentsMidi10Connection,
};
use super::inbox_bindings::Windows::Devices::Midi2::ServiceConfig::MidiServiceTransportPluginConfigManager;
use super::inbox_bindings::Windows::Devices::Midi2::Transports::Virtual::{
    MidiVirtualDevice, MidiVirtualDeviceCreationConfig, MidiVirtualDeviceManager,
};
use super::inbox_bindings::Windows::Devices::Midi2::{
    IMidiMessageReceivedEventSource, MidiApi, MidiEndpointConnection, MidiGroup,
    MidiMessageProcessingPluginAddResult, MidiMessageReceivedEventArgs, MidiSession,
};
use super::{
    MANUFACTURER, Receivers, SESSION_NAME, check_sent, connector_directions, creation_refused,
};
use crate::error::PlatformError;
use std::sync::Arc;
use windows::Foundation::TypedEventHandler;
use windows_core::HSTRING;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegOpenKeyExW,
};

/// Where Windows registers the API's session class when it carries the API.
const REGISTERED: &str =
    r"SOFTWARE\Microsoft\WindowsRuntime\ActivatableClassId\Windows.Devices.Midi2.MidiSession";

/// Converts a Windows error into the platform error for `operation`.
fn failed(operation: &'static str) -> impl Fn(windows_core::Error) -> PlatformError {
    move |error| PlatformError::Os {
        operation,
        detail: error.message(),
    }
}

/// Reports whether Windows carries this form of the API.
///
/// Asked of the registry rather than by activating a class: a copy of the preview API beside the
/// program activates on a Windows whose service predates it, and creating a device through it
/// then waited on the service for good (R-093).
pub fn present() -> bool {
    let key = crate::dll::wide(REGISTERED);
    let mut handle: HKEY = std::ptr::null_mut();
    // SAFETY: the key name is NUL-terminated and outlives the call, and `handle` is written only
    // on success, when it is closed at once.
    unsafe {
        let opened = RegOpenKeyExW(HKEY_LOCAL_MACHINE, key.as_ptr(), 0, KEY_READ, &mut handle);
        if opened == 0 {
            RegCloseKey(handle);
            true
        } else {
            false
        }
    }
}

/// Reports whether the service runs and offers virtual devices.
pub fn usable() -> bool {
    MidiApi::EnsureServiceAvailable().unwrap_or(false)
        && MidiVirtualDeviceManager::IsTransportAvailable().unwrap_or(false)
}

/// A session with the service.
pub struct Service {
    session: MidiSession,
}

impl Service {
    /// Starts the service if it is not running and opens a session with it.
    pub fn connect() -> Result<Self, PlatformError> {
        if !MidiApi::EnsureServiceAvailable().map_err(failed("reach Windows MIDI Services"))? {
            return Err(PlatformError::Unsupported(
                "a virtual MIDI port without Windows MIDI Services",
            ));
        }
        let session = MidiSession::Create(&HSTRING::from(SESSION_NAME))
            .map_err(failed("open a Windows MIDI Services session"))?
            .ok_or_else(|| PlatformError::Os {
                operation: "open a Windows MIDI Services session",
                detail: "the service refused the session".to_owned(),
            })?;
        Ok(Self { session })
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        let session = self.session.clone();
        let _ = super::bounded(
            "close a Windows MIDI Services session",
            super::SERVICE_WAIT,
            move || {
                let _ = session.Close();
            },
        );
    }
}

/// One virtual port, made through the in-box API.
pub struct VirtualPort {
    connection: MidiEndpointConnection,
    /// Kept until the connection is closed: Microsoft's sample releases the device only after
    /// disconnecting, and the service expects it to outlive the connection.
    device: MidiVirtualDevice,
    token: i64,
    sends: Vec<bool>,
    _receivers: Arc<Receivers>,
}

impl VirtualPort {
    /// Creates and publishes the port; see `super::VirtualPort::create`.
    pub(super) fn create(
        service: &Service,
        name: &str,
        names: &[String],
        inputs: u8,
        outputs: u8,
        receivers: Receivers,
    ) -> Result<Self, PlatformError> {
        // Describe the endpoint.
        let info = MidiDeclaredEndpointInfo::new().map_err(failed("describe a virtual port"))?;
        info.SetName(&HSTRING::from(name));
        info.SetProductInstanceId(&HSTRING::from(
            super::super::winmm_identity::virtual_device_id(name).as_str(),
        ));
        info.SetSupportsMidi10Protocol(true);
        info.SetSupportsMidi20Protocol(false);
        info.SetHasStaticFunctionBlocks(true);
        info.SetSpecificationVersionMajor(1);
        info.SetSpecificationVersionMinor(1);
        let config = MidiVirtualDeviceCreationConfig::CreateInstance(
            &HSTRING::from(name),
            &HSTRING::from(name),
            &HSTRING::from(MANUFACTURER),
            &info,
        )
        .map_err(failed("describe a virtual port"))?;
        let blocks = config
            .FunctionBlocks()
            .ok_or_else(|| creation_refused(name, "the configuration has no function blocks"))?;

        // One function block per connector, each on its own group.
        for (index, receives, sends) in connector_directions(inputs, outputs) {
            let block = MidiFunctionBlock::new().map_err(failed("describe a connector"))?;
            block.SetNumber(index);
            block.SetIsActive(true);
            let label = names.get(usize::from(index)).map_or(name, String::as_str);
            block.SetName(&HSTRING::from(label));
            let group = MidiGroup::CreateInstance(index).map_err(failed("describe a connector"))?;
            block.SetFirstGroup(&group);
            block.SetGroupCount(1);
            block.SetDirection(match (receives, sends) {
                (true, true) => MidiFunctionBlockDirection::Bidirectional,
                (true, false) => MidiFunctionBlockDirection::BlockInput,
                _ => MidiFunctionBlockDirection::BlockOutput,
            });
            block.SetRepresentsMidi10Connection(
                MidiFunctionBlockRepresentsMidi10Connection::YesBandwidthUnrestricted,
            );
            blocks
                .Append(&block)
                .map_err(failed("describe a connector"))?;
        }

        // Create the device and connect to its own side.
        let device = match MidiVirtualDeviceManager::CreateVirtualDevice(&config)
            .map_err(failed("create a virtual port"))?
        {
            Some(device) => device,
            None => return Err(creation_refused(name, &service_answer(&config))),
        };
        let connection = service
            .session
            .CreateEndpointConnection(&device.DeviceEndpointDeviceId())
            .ok_or_else(|| creation_refused(name, "no connection to the device was made"))?;

        // Receive what other applications send.
        let receivers = Arc::new(receivers);
        let delivering = Arc::clone(&receivers);
        let handler = TypedEventHandler::<
            IMidiMessageReceivedEventSource,
            MidiMessageReceivedEventArgs,
        >::new(move |_, args| {
            if let Some(args) = args.as_ref() {
                received(&delivering, args);
            }
            Ok(())
        });
        let token = connection
            .MessageReceived(&handler)
            .map_err(failed("listen to a virtual port"))?;

        // Without the device added to the connection, the port sends but never receives.
        if connection.AddMessageProcessingPlugin(&device)
            != MidiMessageProcessingPluginAddResult::Succeeded
        {
            let _ = connection.RemoveMessageReceived(token);
            return Err(creation_refused(
                name,
                "the device could not join its connection",
            ));
        }
        if !connection.Open() {
            let _ = connection.RemoveMessageReceived(token);
            return Err(creation_refused(name, "the connection did not open"));
        }
        Ok(Self {
            connection,
            device,
            token,
            sends: connector_directions(inputs, outputs)
                .map(|(_, _, sends)| sends)
                .collect(),
            _receivers: receivers,
        })
    }

    /// Reports whether connector `connector` sends to other applications.
    pub(super) fn sends(&self, connector: u8) -> bool {
        self.sends
            .get(usize::from(connector))
            .copied()
            .unwrap_or(false)
    }

    /// Sends one 32-bit packet.
    pub(super) fn send_word(&self, word: u32) -> Result<(), PlatformError> {
        check_sent(self.connection.SendSingleMessageWords(0, word).0)
    }

    /// Sends one 64-bit packet.
    pub(super) fn send_words(&self, first: u32, second: u32) -> Result<(), PlatformError> {
        check_sent(self.connection.SendSingleMessageWords2(0, first, second).0)
    }

    /// Withdraws the port from other applications.
    pub(super) fn close(self, service: &Service) {
        let _ = self.connection.RemoveMessageReceived(self.token);
        service
            .session
            .DisconnectEndpointConnection(self.connection.ConnectionId());
        drop(self.device);
    }
}

/// Reads one packet another application sent and hands it on. Real-time: nothing allocated.
fn received(receivers: &Receivers, args: &MidiMessageReceivedEventArgs) {
    let mut words = [0u32; 4];
    let [first, second, third, fourth] = &mut words;
    let count = usize::from(args.FillWords(first, second, third, fourth)).min(words.len());
    receivers.deliver(words.get(..count).unwrap_or_default());
}

/// Asks the service again, directly, why it refused a port, since the API keeps the answer to
/// itself.
fn service_answer(config: &MidiVirtualDeviceCreationConfig) -> String {
    match MidiServiceTransportPluginConfigManager::SendUpdate(config) {
        Ok(Some(response)) => format!(
            "the service refused it with status {} and error {:#x} {}",
            response.Status().0,
            response.ServiceErrorCode(),
            response.ServiceErrorMessage()
        ),
        Ok(None) => "the service gave no answer".to_owned(),
        Err(error) => format!("the service could not be asked: {}", error.message()),
    }
}
