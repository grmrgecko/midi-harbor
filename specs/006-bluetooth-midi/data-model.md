# Data Model: Bluetooth MIDI

Part of the domain model; [identity and the endpoint](../001-service-and-clients/data-model.md) are
shared by every spec. Types live in `midi-harbor-core` unless noted.

---

## BluetoothDevice (FR-016..021)

```
BluetoothDevice {
    address:       BluetoothAddress
    role:          BleRole            // Central | Peripheral
    rssi:          Option<i16>
    paired:        bool
    last_seen:     Option<Timestamp>
    timestamp_base: Option<BleClockBase>  // 13-bit wraparound tracking (FR-019)
}
```
