//! Virtual ports through the App SDK, `Microsoft.Windows.Devices.Midi2`.
//!
//! The App SDK works with the service Windows already ships, once the user has installed its
//! runtime. A program reaches the runtime through a COM initializer the runtime registers, which
//! has to be created before any of the API's classes and kept for as long as they are used.

use super::appsdk_bindings::Microsoft::Windows::Devices::Midi2::Endpoints::Virtual::{
    MidiVirtualDevice, MidiVirtualDeviceCreationConfig, MidiVirtualDeviceManager,
};
use super::appsdk_bindings::Microsoft::Windows::Devices::Midi2::{
    IMidiMessageReceivedEventSource, MidiDeclaredEndpointInfo, MidiEndpointConnection,
    MidiFunctionBlock, MidiFunctionBlockDirection, MidiFunctionBlockRepresentsMidi10Connection,
    MidiGroup, MidiMessageReceivedEventArgs, MidiSession,
};
use super::{
    MANUFACTURER, Receivers, SESSION_NAME, check_sent, connector_directions, creation_refused,
};
use crate::error::PlatformError;
use std::ffi::c_void;
use std::sync::{Arc, OnceLock};
use windows::Foundation::TypedEventHandler;
use windows_core::{GUID, HRESULT, HSTRING};
use windows_sys::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};

/// The runtime's initializer class, `MidiClientInitializer`.
const INITIALIZER_CLASS: GUID = GUID::from_u128(0xc3263827_c3b0_bdbd_2500_ce63a3f3f2c3);

/// The initializer's interface, `IMidiClientInitializer`.
const INITIALIZER_INTERFACE: GUID = GUID::from_u128(0x8087b303_d551_bce2_1ead_a2500d50c580);

/// Asks COM to create the object in the caller's own context, as the SDK's header does.
const CLSCTX_FROM_DEFAULT_CONTEXT: u32 = 0x0002_0000;

/// The initializer's interface as the SDK's header declares it: `IUnknown`'s three methods, then
/// its own two.
#[repr(C)]
struct InitializerVtbl {
    query_interface: usize,
    add_ref: usize,
    release: usize,
    installed_version: usize,
    ensure_service_available: unsafe extern "system" fn(*mut c_void) -> HRESULT,
}

/// The initializer, created once and kept for the life of the process.
#[derive(Clone, Copy)]
struct Initializer(*mut c_void);

// SAFETY: the object is created in the multithreaded apartment and is never released, so any
// thread may call it for as long as the process runs.
unsafe impl Send for Initializer {}
// SAFETY: as above.
unsafe impl Sync for Initializer {}

impl Initializer {
    /// Returns the initializer, creating it the first time, or nothing if the runtime is absent.
    fn get() -> Option<Self> {
        static INITIALIZER: OnceLock<Option<Initializer>> = OnceLock::new();
        *INITIALIZER.get_or_init(|| {
            let mut object: *mut c_void = std::ptr::null_mut();
            // SAFETY: the class and interface identifiers are the runtime's, and `object` is
            // written only on success.
            let result = unsafe {
                CoCreateInstance(
                    std::ptr::from_ref(&INITIALIZER_CLASS).cast(),
                    std::ptr::null_mut(),
                    CLSCTX_INPROC_SERVER | CLSCTX_FROM_DEFAULT_CONTEXT,
                    std::ptr::from_ref(&INITIALIZER_INTERFACE).cast(),
                    &mut object,
                )
            };
            (result >= 0 && !object.is_null()).then_some(Self(object))
        })
    }

    /// Starts the service if it is not running, reporting whether it is.
    fn ensure_service_available(self) -> bool {
        // SAFETY: the object is a live IMidiClientInitializer, whose vtable is laid out as
        // `InitializerVtbl` declares.
        unsafe {
            let vtable = *self.0.cast::<*const InitializerVtbl>();
            ((*vtable).ensure_service_available)(self.0).is_ok()
        }
    }
}

/// Converts a Windows error into the platform error for `operation`.
fn failed(operation: &'static str) -> impl Fn(windows_core::Error) -> PlatformError {
    move |error| PlatformError::Os {
        operation,
        detail: error.message(),
    }
}

/// Describes why the service refused a step.
///
/// A refusal comes back as no object at all, which the bindings report as an error whose code is
/// success, and whose message would say the operation completed successfully.
fn refusal(error: &windows_core::Error) -> String {
    if error.code().is_ok() {
        "the service refused it without saying why".to_owned()
    } else {
        error.message()
    }
}

/// Reports whether the runtime is installed, the service runs, and it offers virtual devices.
pub fn usable() -> bool {
    Initializer::get().is_some_and(Initializer::ensure_service_available)
        && MidiVirtualDeviceManager::IsTransportAvailable().unwrap_or(false)
}

/// A session with the service.
pub struct Service {
    session: MidiSession,
}

impl Service {
    /// Starts the runtime and the service, and opens a session with it.
    pub fn connect() -> Result<Self, PlatformError> {
        let initializer = Initializer::get().ok_or(PlatformError::Unsupported(
            "a virtual MIDI port without Windows MIDI Services",
        ))?;
        if !initializer.ensure_service_available() {
            return Err(PlatformError::Os {
                operation: "reach Windows MIDI Services",
                detail: "the service did not start".to_owned(),
            });
        }
        let session = MidiSession::Create(&HSTRING::from(SESSION_NAME))
            .map_err(failed("open a Windows MIDI Services session"))?;
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

/// One virtual port, made through the App SDK.
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
        let info = MidiDeclaredEndpointInfo {
            Name: HSTRING::from(name),
            ProductInstanceId: HSTRING::from(
                super::super::winmm_identity::virtual_device_id(name).as_str(),
            ),
            SupportsMidi10Protocol: true,
            SupportsMidi20Protocol: false,
            SupportsReceivingJitterReductionTimestamps: false,
            SupportsSendingJitterReductionTimestamps: false,
            HasStaticFunctionBlocks: true,
            DeclaredFunctionBlockCount: inputs.max(outputs),
            SpecificationVersionMajor: 1,
            SpecificationVersionMinor: 1,
        };
        let describe = failed("describe a virtual port");
        let config = MidiVirtualDeviceCreationConfig::CreateInstance(
            &HSTRING::from(name),
            &HSTRING::from(name),
            &HSTRING::from(MANUFACTURER),
            &info,
        )
        .map_err(&describe)?;
        let blocks = config.FunctionBlocks().map_err(&describe)?;
        // One function block per connector, each on its own group.
        let connector = failed("describe a connector");
        for (index, receives, sends) in connector_directions(inputs, outputs) {
            let block = MidiFunctionBlock::new().map_err(&connector)?;
            block.SetNumber(index).map_err(&connector)?;
            block.SetIsActive(true).map_err(&connector)?;
            let label = names.get(usize::from(index)).map_or(name, String::as_str);
            block.SetName(&HSTRING::from(label)).map_err(&connector)?;
            let group = MidiGroup::CreateInstance(index).map_err(&connector)?;
            block.SetFirstGroup(&group).map_err(&connector)?;
            block.SetGroupCount(1).map_err(&connector)?;
            block
                .SetDirection(match (receives, sends) {
                    (true, true) => MidiFunctionBlockDirection::Bidirectional,
                    (true, false) => MidiFunctionBlockDirection::BlockInput,
                    _ => MidiFunctionBlockDirection::BlockOutput,
                })
                .map_err(&connector)?;
            block
                .SetRepresentsMidi10Connection(
                    MidiFunctionBlockRepresentsMidi10Connection::YesBandwidthUnrestricted,
                )
                .map_err(&connector)?;
            blocks.Append(&block).map_err(&connector)?;
        }
        // Create the device and connect to its own side.
        let device = MidiVirtualDeviceManager::CreateVirtualDevice(&config)
            .map_err(|error| creation_refused(name, &refusal(&error)))?;
        let device_id = device
            .DeviceEndpointDeviceId()
            .map_err(failed("create a virtual port"))?;
        let connection = service
            .session
            .CreateEndpointConnection(&device_id)
            .map_err(|error| creation_refused(name, &refusal(&error)))?;
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
        if let Err(error) = connection.AddMessageProcessingPlugin(&device) {
            let _ = connection.RemoveMessageReceived(token);
            return Err(creation_refused(name, &error.message()));
        }
        if !connection.Open().unwrap_or(false) {
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
        let result = self
            .connection
            .SendSingleMessageWords(0, word)
            .map_err(failed("send through a virtual port"))?;
        check_sent(result.0)
    }

    /// Sends one 64-bit packet.
    pub(super) fn send_words(&self, first: u32, second: u32) -> Result<(), PlatformError> {
        let result = self
            .connection
            .SendSingleMessageWords2(0, first, second)
            .map_err(failed("send through a virtual port"))?;
        check_sent(result.0)
    }

    /// Withdraws the port from other applications.
    pub(super) fn close(self, service: &Service) {
        let _ = self.connection.RemoveMessageReceived(self.token);
        if let Ok(id) = self.connection.ConnectionId() {
            let _ = service.session.DisconnectEndpointConnection(id);
        }
        drop(self.device);
    }
}

/// Reads one packet another application sent and hands it on. Real-time: nothing allocated.
fn received(receivers: &Receivers, args: &MidiMessageReceivedEventArgs) {
    let mut words = [0u32; 4];
    let [first, second, third, fourth] = &mut words;
    let count =
        usize::from(args.FillWords(first, second, third, fourth).unwrap_or(0)).min(words.len());
    receivers.deliver(words.get(..count).unwrap_or_default());
}
