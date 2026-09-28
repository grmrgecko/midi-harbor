//! MIDI hardware and other applications' ports, through WinMM.
//!
//! WinMM is the one MIDI interface every Windows release has, and every other interface's ports
//! appear in it: USB and Bluetooth devices, and the ports loopMIDI or rtpMIDI create. It delivers
//! input to a callback on a system thread, which is the real-time path here: the callback scans
//! bytes into the endpoint's ring and hands buffers back, and does nothing else.

use super::winmm_identity::{self, WinmmEndpoint};
use crate::dll::narrow;
use crate::error::PlatformError;
use midi_harbor_core::midi::MidiMessage;
use midi_harbor_core::rtchannel::RtProducer;
use midi_harbor_core::stream::{Chunk, Scanner};
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use windows_sys::Win32::Media::Audio::{
    CALLBACK_FUNCTION, CALLBACK_NULL, HMIDIIN, HMIDIOUT, MHDR_DONE, MIDIHDR, MIDIINCAPSW,
    MIDIOUTCAPSW, midiInAddBuffer, midiInClose, midiInGetDevCapsW, midiInGetErrorTextW,
    midiInGetID, midiInGetNumDevs, midiInMessage, midiInOpen, midiInPrepareHeader, midiInReset,
    midiInStart, midiInStop, midiInUnprepareHeader, midiOutClose, midiOutGetDevCapsW, midiOutGetID,
    midiOutGetNumDevs, midiOutLongMsg, midiOutMessage, midiOutOpen, midiOutPrepareHeader,
    midiOutReset, midiOutShortMsg, midiOutUnprepareHeader,
};
use windows_sys::Win32::Media::Multimedia::{
    DRV_QUERYDEVICEINTERFACE, DRV_QUERYDEVICEINTERFACESIZE,
};
use windows_sys::Win32::Media::{
    MM_MIM_DATA, MM_MIM_LONGDATA, MMSYSERR_ALLOCATED, MMSYSERR_BADDEVICEID, MMSYSERR_INVALHANDLE,
    MMSYSERR_NODRIVER, MMSYSERR_NOERROR,
};

/// How many system-exclusive buffers each input keeps queued with WinMM.
///
/// While the callback scans one, the others keep receiving, so a dump arriving faster than the
/// callback returns is not cut short.
const SYSEX_BUFFERS: usize = 4;

/// Size of each system-exclusive buffer.
///
/// A dump longer than this arrives in several buffers, and the scanner joins them.
const SYSEX_BUFFER_SIZE: usize = 4096;

/// How long a system-exclusive send may take per byte: MIDI's 31250 baud carries 3125 bytes a
/// second, 320 microseconds each.
const SYSEX_TIME_PER_BYTE: Duration = Duration::from_micros(320);

/// Time allowed for a system-exclusive send on top of what its length needs.
const SYSEX_SLACK: Duration = Duration::from_secs(1);

/// How often a system-exclusive send is looked at while it goes out.
const SYSEX_POLL: Duration = Duration::from_millis(1);

/// Size of a `MIDIHDR`, as WinMM's functions ask for it.
const HEADER_SIZE: u32 = std::mem::size_of::<MIDIHDR>() as u32;

/// How many times the lists are read while they keep changing between reads.
const ENUMERATE_ATTEMPTS: usize = 3;

/// Lists every WinMM input and output now present.
///
/// WinMM is read by position, so a device arriving or leaving part way through shifts what the
/// remaining positions hold, and one reading can miss an endpoint or show one twice. The lists
/// are read until two readings agree, or the attempts run out and the last reading stands.
pub fn enumerate() -> (Vec<WinmmEndpoint>, Vec<WinmmEndpoint>) {
    let mut previous = read_lists();
    for _ in 1..ENUMERATE_ATTEMPTS {
        let current = read_lists();
        if current == previous {
            return current;
        }
        previous = current;
    }
    previous
}

/// Reads both lists once.
fn read_lists() -> (Vec<WinmmEndpoint>, Vec<WinmmEndpoint>) {
    // SAFETY: takes no arguments and only reads the system's device count.
    let input_count = unsafe { midiInGetNumDevs() };
    // SAFETY: as above.
    let output_count = unsafe { midiOutGetNumDevs() };
    let inputs = (0..input_count)
        .filter_map(|index| endpoint_at(index, true))
        .collect();
    let outputs = (0..output_count)
        .filter_map(|index| endpoint_at(index, false))
        .collect();
    (inputs, outputs)
}

/// Describes the endpoint at one position in WinMM's list now, or nothing if there is none.
pub fn endpoint_at(index: u32, input: bool) -> Option<WinmmEndpoint> {
    let name = if input {
        let mut caps = MIDIINCAPSW::default();
        let size = std::mem::size_of::<MIDIINCAPSW>() as u32;
        // SAFETY: `caps` is a writable MIDIINCAPSW of the size passed.
        let result = unsafe { midiInGetDevCapsW(index as usize, &mut caps, size) };
        let name = caps.szPname;
        (result == MMSYSERR_NOERROR).then(|| narrow(&name))?
    } else {
        let mut caps = MIDIOUTCAPSW::default();
        let size = std::mem::size_of::<MIDIOUTCAPSW>() as u32;
        // SAFETY: `caps` is a writable MIDIOUTCAPSW of the size passed.
        let result = unsafe { midiOutGetDevCapsW(index as usize, &mut caps, size) };
        let name = caps.szPname;
        (result == MMSYSERR_NOERROR).then(|| narrow(&name))?
    };
    Some(WinmmEndpoint {
        index,
        name,
        interface: interface_path(index, input),
    })
}

/// Asks WinMM for an endpoint's device interface path, or returns an empty string if it has none.
fn interface_path(index: u32, input: bool) -> String {
    // WinMM accepts a device index in place of a handle for these two queries.
    let device = index as usize as *mut core::ffi::c_void;
    let mut size: u32 = 0;
    let size_address = &mut size as *mut u32 as usize;
    // SAFETY: the query writes the path's size in bytes to `size`, which outlives the call.
    let result = unsafe {
        if input {
            midiInMessage(device, DRV_QUERYDEVICEINTERFACESIZE, size_address, 0)
        } else {
            midiOutMessage(device, DRV_QUERYDEVICEINTERFACESIZE, size_address, 0)
        }
    };
    if result != MMSYSERR_NOERROR || size == 0 {
        return String::new();
    }
    let units = (size as usize).div_ceil(2);
    let mut buffer = vec![0u16; units];
    // SAFETY: `buffer` holds at least `size` bytes, which is what the query writes.
    let result = unsafe {
        if input {
            midiInMessage(
                device,
                DRV_QUERYDEVICEINTERFACE,
                buffer.as_mut_ptr() as usize,
                size as usize,
            )
        } else {
            midiOutMessage(
                device,
                DRV_QUERYDEVICEINTERFACE,
                buffer.as_mut_ptr() as usize,
                size as usize,
            )
        }
    };
    if result != MMSYSERR_NOERROR {
        return String::new();
    }
    narrow(&buffer)
}

/// Turns a WinMM result into the platform error that says what it means for the user.
fn failure(operation: &'static str, result: u32) -> PlatformError {
    match result {
        MMSYSERR_ALLOCATED => PlatformError::Claimed { by: None },
        MMSYSERR_BADDEVICEID | MMSYSERR_NODRIVER | MMSYSERR_INVALHANDLE => {
            PlatformError::NotFound(format!("the device ({operation})"))
        }
        _ => {
            let mut text = [0u16; 256];
            // SAFETY: `text` is writable for the length passed. Input and output share one set of
            // error texts.
            let described =
                unsafe { midiInGetErrorTextW(result, text.as_mut_ptr(), text.len() as u32) };
            let detail = if described == MMSYSERR_NOERROR {
                narrow(&text)
            } else {
                format!("winmm error {result}")
            };
            PlatformError::Os { operation, detail }
        }
    }
}

/// What the input callback owns while an input is open.
struct Live {
    scanner: Scanner,
    sink: Option<RtProducer>,
}

/// Everything an open input shares with its callback, at an address that does not move.
///
/// The callback is the only code that touches `live` while the input is open, and WinMM does not
/// run two callbacks for one input at once; `closing` is the only field both sides use.
struct InputContext {
    closing: AtomicBool,
    live: UnsafeCell<Live>,
    headers: UnsafeCell<[MIDIHDR; SYSEX_BUFFERS]>,
    buffers: UnsafeCell<[[u8; SYSEX_BUFFER_SIZE]; SYSEX_BUFFERS]>,
}

/// Why an input did not open, with the sink it was to deliver into.
pub struct OpenFailure {
    /// What went wrong.
    pub error: PlatformError,
    /// The sink, handed back unused.
    pub sink: Option<RtProducer>,
}

/// An open WinMM input.
pub struct Input {
    handle: HMIDIIN,
    context: *mut InputContext,
}

// SAFETY: a WinMM handle may be used from any thread, and the context is reached from other
// threads only through the atomic flag until `close` takes it back.
unsafe impl Send for Input {}

impl Input {
    /// Opens WinMM input `index` and starts delivering what it receives into `sink`.
    ///
    /// A failure hands the sink back, so the caller can try another input with it.
    pub fn open(index: u32, sink: Option<RtProducer>) -> Result<Self, Box<OpenFailure>> {
        // Build the context before opening, so the callback never sees it half made.
        let context = Box::into_raw(Box::new(InputContext {
            closing: AtomicBool::new(false),
            live: UnsafeCell::new(Live {
                scanner: Scanner::new(),
                sink,
            }),
            headers: UnsafeCell::new(std::array::from_fn(|_| MIDIHDR::default())),
            buffers: UnsafeCell::new([[0u8; SYSEX_BUFFER_SIZE]; SYSEX_BUFFERS]),
        }));

        let mut handle: HMIDIIN = std::ptr::null_mut();
        // SAFETY: `input_callback` has the signature WinMM calls, and `context` stays valid until
        // `close` has closed the handle.
        let result = unsafe {
            midiInOpen(
                &mut handle,
                index,
                input_callback as unsafe extern "system" fn(HMIDIIN, u32, usize, usize, usize)
                    as usize,
                context as usize,
                CALLBACK_FUNCTION,
            )
        };
        if result != MMSYSERR_NOERROR {
            // SAFETY: the open failed, so no callback holds the context.
            let sink = unsafe { Box::from_raw(context) }.live.into_inner().sink;
            return Err(Box::new(OpenFailure {
                error: failure("open a MIDI input", result),
                sink,
            }));
        }
        let input = Self { handle, context };

        // Queue the system-exclusive buffers, then start.
        let queued = || -> Result<(), PlatformError> {
            // SAFETY: the context is alive, and its headers and buffers are not yet shared with
            // WinMM; each is handed over only once prepared.
            unsafe {
                let headers = &mut *(*context).headers.get();
                let buffers = &mut *(*context).buffers.get();
                for (header, buffer) in headers.iter_mut().zip(buffers.iter_mut()) {
                    header.lpData = buffer.as_mut_ptr();
                    header.dwBufferLength = SYSEX_BUFFER_SIZE as u32;
                    let prepared = midiInPrepareHeader(handle, header, HEADER_SIZE);
                    if prepared != MMSYSERR_NOERROR {
                        return Err(failure("prepare a MIDI input buffer", prepared));
                    }
                    let added = midiInAddBuffer(handle, header, HEADER_SIZE);
                    if added != MMSYSERR_NOERROR {
                        return Err(failure("queue a MIDI input buffer", added));
                    }
                }
            }
            Ok(())
        };
        if let Err(error) = queued() {
            return Err(Box::new(OpenFailure {
                error,
                sink: input.close_returning_sink(),
            }));
        }
        // SAFETY: the handle is open.
        let started = unsafe { midiInStart(handle) };
        if started != MMSYSERR_NOERROR {
            let sink = input.close_returning_sink();
            return Err(Box::new(OpenFailure {
                error: failure("start a MIDI input", started),
                sink,
            }));
        }
        Ok(input)
    }

    /// Returns where in WinMM's list the device this input has open is now.
    pub fn position(&self) -> Option<u32> {
        let mut index = 0u32;
        // SAFETY: the handle is open and `index` is writable.
        let result = unsafe { midiInGetID(self.handle, &mut index) };
        (result == MMSYSERR_NOERROR).then_some(index)
    }

    /// Stops and closes the input, and frees what its callback used.
    pub fn close(self) {
        drop(self.close_returning_sink());
    }

    /// Stops and closes the input, and returns the sink its callback was delivering into.
    pub fn close_returning_sink(self) -> Option<RtProducer> {
        // SAFETY: the context is alive until it is freed at the end of this function, and the
        // flag is safe to set from any thread.
        unsafe { (*self.context).closing.store(true, Ordering::Release) };
        // SAFETY: the handle is open; resetting returns every queued buffer, which the callback
        // does not queue again once `closing` is set, and closing ends the callbacks.
        unsafe {
            midiInStop(self.handle);
            midiInReset(self.handle);
            let headers = &mut *(*self.context).headers.get();
            for header in headers.iter_mut() {
                if !header.lpData.is_null() {
                    midiInUnprepareHeader(self.handle, header, HEADER_SIZE);
                }
            }
            midiInClose(self.handle);
            Box::from_raw(self.context).live.into_inner().sink
        }
    }
}

/// Receives what an open input delivers.
///
/// A real-time context: it scans bytes into the ring and hands buffers back, and allocates,
/// locks and logs nothing.
unsafe extern "system" fn input_callback(
    handle: HMIDIIN,
    message: u32,
    instance: usize,
    first: usize,
    _second: usize,
) {
    let context = instance as *const InputContext;
    // SAFETY: WinMM passes back the context given to midiInOpen, which lives until the handle is
    // closed, and no callback runs after that.
    let Some(context) = (unsafe { context.as_ref() }) else {
        return;
    };
    match message {
        MM_MIM_DATA => {
            let bytes = winmm_identity::unpack(first);
            let [status, ..] = bytes;
            let length = winmm_identity::short_length(status);
            // SAFETY: only this callback touches `live` while the input is open.
            let live = unsafe { &mut *context.live.get() };
            if let (Some(sink), Some(message)) = (live.sink.as_mut(), bytes.get(..length)) {
                scan(&mut live.scanner, sink, message);
            }
        }
        MM_MIM_LONGDATA => {
            let header = first as *mut MIDIHDR;
            // SAFETY: WinMM hands back one of the headers queued on this input, whose buffer
            // holds `dwBytesRecorded` bytes.
            unsafe {
                let Some(filled) = header.as_ref() else {
                    return;
                };
                let recorded = (filled.dwBytesRecorded as usize).min(SYSEX_BUFFER_SIZE);
                let bytes = std::slice::from_raw_parts(filled.lpData.cast_const(), recorded);
                let live = &mut *context.live.get();
                if let Some(sink) = live.sink.as_mut() {
                    scan(&mut live.scanner, sink, bytes);
                }
                // A buffer returned while closing is being handed back for good.
                if !context.closing.load(Ordering::Acquire) {
                    midiInAddBuffer(handle, header, HEADER_SIZE);
                }
            }
        }
        _ => {}
    }
}

/// Splits bytes into messages and pushes each one. Safe on the real-time path.
fn scan(scanner: &mut Scanner, sink: &mut RtProducer, bytes: &[u8]) {
    scanner.scan(bytes, &mut |chunk| match chunk {
        Chunk::Message(message) => {
            let _ = sink.push(message, 0);
        }
        Chunk::SysEx { bytes, end } => {
            let _ = sink.push_sysex(bytes, end, 0);
        }
    });
}

/// An open WinMM output.
pub struct Output {
    handle: HMIDIOUT,
}

// SAFETY: a WinMM output handle may be used from any thread.
unsafe impl Send for Output {}

impl Output {
    /// Opens WinMM output `index`.
    pub fn open(index: u32) -> Result<Self, PlatformError> {
        let mut handle: HMIDIOUT = std::ptr::null_mut();
        // SAFETY: `handle` is writable, and no callback is asked for.
        let result = unsafe { midiOutOpen(&mut handle, index, 0, 0, CALLBACK_NULL) };
        if result != MMSYSERR_NOERROR {
            return Err(failure("open a MIDI output", result));
        }
        Ok(Self { handle })
    }

    /// Returns where in WinMM's list the device this output has open is now.
    pub fn position(&self) -> Option<u32> {
        let mut index = 0u32;
        // SAFETY: the handle is open and `index` is writable.
        let result = unsafe { midiOutGetID(self.handle, &mut index) };
        (result == MMSYSERR_NOERROR).then_some(index)
    }

    /// Sends messages, each as one short message.
    pub fn send(&self, messages: &[MidiMessage]) -> Result<(), PlatformError> {
        let mut buffer = [0u8; 3];
        for message in messages {
            let written = message.encode(&mut buffer);
            let Some(bytes) = buffer.get(..written) else {
                continue;
            };
            if bytes.is_empty() {
                continue;
            }
            // SAFETY: the handle is open.
            let result = unsafe { midiOutShortMsg(self.handle, winmm_identity::pack(bytes)) };
            if result != MMSYSERR_NOERROR {
                return Err(failure("send to a MIDI output", result));
            }
        }
        Ok(())
    }

    /// Sends one whole system-exclusive message and waits for it to go out.
    pub fn send_sysex(&self, bytes: &[u8]) -> Result<(), PlatformError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut data = bytes.to_vec();
        let mut header = Box::new(MIDIHDR {
            lpData: data.as_mut_ptr(),
            dwBufferLength: u32::try_from(data.len()).map_err(|_| PlatformError::Os {
                operation: "send a system-exclusive message",
                detail: "the message is larger than WinMM accepts".to_owned(),
            })?,
            dwBytesRecorded: 0,
            ..MIDIHDR::default()
        });
        let header_ptr: *mut MIDIHDR = &mut *header;

        // SAFETY: the header and its data stay alive and in place until unprepared below.
        let prepared = unsafe { midiOutPrepareHeader(self.handle, header_ptr, HEADER_SIZE) };
        if prepared != MMSYSERR_NOERROR {
            return Err(failure("prepare a system-exclusive message", prepared));
        }
        // SAFETY: as above.
        let sent = unsafe { midiOutLongMsg(self.handle, header_ptr, HEADER_SIZE) };
        let outcome = if sent == MMSYSERR_NOERROR {
            // Wait for WinMM to finish with the buffer, allowing what the length needs at MIDI's
            // own rate.
            let bound = SYSEX_TIME_PER_BYTE
                .saturating_mul(u32::try_from(data.len()).unwrap_or(u32::MAX))
                .saturating_add(SYSEX_SLACK);
            let started = Instant::now();
            loop {
                // SAFETY: WinMM sets the flag from its own thread, so it is read without caching.
                let flags = unsafe { std::ptr::addr_of!((*header_ptr).dwFlags).read_volatile() };
                if flags & MHDR_DONE != 0 {
                    break Ok(());
                }
                if started.elapsed() >= bound {
                    // SAFETY: the handle is open; resetting returns the buffer.
                    unsafe { midiOutReset(self.handle) };
                    break Err(PlatformError::Os {
                        operation: "send a system-exclusive message",
                        detail: "the device did not take it in time".to_owned(),
                    });
                }
                std::thread::sleep(SYSEX_POLL);
            }
        } else {
            Err(failure("send a system-exclusive message", sent))
        };
        // SAFETY: WinMM has finished with the buffer, so it may be unprepared and freed.
        unsafe { midiOutUnprepareHeader(self.handle, header_ptr, HEADER_SIZE) };
        drop(header);
        drop(data);
        outcome
    }

    /// Closes the output.
    pub fn close(self) {
        // SAFETY: the handle is open and is not used again.
        unsafe {
            midiOutReset(self.handle);
            midiOutClose(self.handle);
        }
    }
}
