# MTP/PTP protocol reference

This document covers the essential protocol details for working on `mtp-rs`. For the full MTP specification, see `docs/mtp-v1_1-spec/`.

## Byte order

All multi-byte values in MTP/PTP are **little-endian**.

```rust
// Encoding a u32 value 0x12345678:
[0x78, 0x56, 0x34, 0x12]  // LSB first
```

## Simple types

| Type Code | Name   | Size (bytes) | Rust Type |
|-----------|--------|--------------|-----------|
| 0x0001    | INT8   | 1            | `i8`      |
| 0x0002    | UINT8  | 1            | `u8`      |
| 0x0003    | INT16  | 2            | `i16`     |
| 0x0004    | UINT16 | 2            | `u16`     |
| 0x0005    | INT32  | 4            | `i32`     |
| 0x0006    | UINT32 | 4            | `u32`     |
| 0x0007    | INT64  | 8            | `i64`     |
| 0x0008    | UINT64 | 8            | `u64`     |
| 0xFFFF    | STR    | Variable     | `String`  |

## Strings

Strings are encoded as:

1. **1 byte**: Number of characters (including null terminator)
2. **N × 2 bytes**: UTF-16LE encoded characters
3. **2 bytes**: Null terminator (0x0000)

**Empty string**: Single byte `0x00`

**Example**: String "Hi"

```
03          // Length: 3 chars (H, i, null)
48 00       // 'H' in UTF-16LE
69 00       // 'i' in UTF-16LE
00 00       // Null terminator
```

**Maximum length**: 255 characters (including null)

## Arrays

Arrays are encoded as:

1. **4 bytes**: Element count (u32, little-endian)
2. **N × element_size**: Elements

**Empty array**: `00 00 00 00`

**Example**: Array of three u32 values [1, 2, 3]

```
03 00 00 00    // Count: 3
01 00 00 00    // Element 0: 1
02 00 00 00    // Element 1: 2
03 00 00 00    // Element 2: 3
```

## USB container format

All MTP/PTP communication uses containers:

### Container header (12 bytes)

| Offset | Size | Field         | Description                             |
|--------|------|---------------|-----------------------------------------|
| 0      | 4    | Length        | Total container size (header + payload) |
| 4      | 2    | Type          | Container type (see below)              |
| 6      | 2    | Code          | Operation/Response/Event code           |
| 8      | 4    | TransactionID | Transaction identifier                  |

### Container types

| Value  | Name     | Direction                 |
|--------|----------|---------------------------|
| 0x0001 | Command  | Host → Device             |
| 0x0002 | Data     | Either direction          |
| 0x0003 | Response | Device → Host             |
| 0x0004 | Event    | Device → Host (interrupt) |

### Command container

```
┌────────────────────────────────────────────────────┐
│ Length (4)  │ Type=1 (2) │ OpCode (2) │ TxID (4)  │
├────────────────────────────────────────────────────┤
│ Param1 (4)  │ Param2 (4) │ Param3 (4) │ ...       │
└────────────────────────────────────────────────────┘
```

- Up to 5 parameters (each 4 bytes)
- Length = 12 + (num_params × 4)

**Example**: OpenSession with SessionID=1

```
10 00 00 00    // Length: 16 bytes
01 00          // Type: Command
02 10          // Code: 0x1002 (OpenSession)
01 00 00 00    // TransactionID: 1
01 00 00 00    // Param1: SessionID = 1
```

### Data container

```
┌────────────────────────────────────────────────────┐
│ Length (4)  │ Type=2 (2) │ OpCode (2) │ TxID (4)  │
├────────────────────────────────────────────────────┤
│                    Payload...                      │
└────────────────────────────────────────────────────┘
```

- Echoes the operation code from command
- Same transaction ID
- Large payloads split across multiple USB packets

### Response container

```
┌────────────────────────────────────────────────────┐
│ Length (4)  │ Type=3 (2) │ RespCode (2)│ TxID (4) │
├────────────────────────────────────────────────────┤
│ Param1 (4)  │ Param2 (4) │ ...                    │
└────────────────────────────────────────────────────┘
```

- Contains response code
- May include up to 5 response parameters

### Event container

```
┌────────────────────────────────────────────────────┐
│ Length (4)  │ Type=4 (2) │ EvtCode (2)│ TxID (4)  │
├────────────────────────────────────────────────────┤
│ Param1 (4)  │ Param2 (4) │ Param3 (4)            │
└────────────────────────────────────────────────────┘
```

- Sent on interrupt endpoint
- Up to 3 parameters
- Can arrive during any phase

## Transaction flow

### Basic operation (no data phase)

```
Host                          Device
  │                              │
  │─── Command Container ───────▶│
  │                              │
  │◀── Response Container ───────│
  │                              │
```

### Operation with data (device → host)

```
Host                          Device
  │                              │
  │─── Command Container ───────▶│
  │                              │
  │◀── Data Container(s) ────────│
  │                              │
  │◀── Response Container ───────│
  │                              │
```

### Operation with data (host → device)

```
Host                          Device
  │                              │
  │─── Command Container ───────▶│
  │                              │
  │─── Data Container(s) ───────▶│
  │                              │
  │◀── Response Container ───────│
  │                              │
```

## Transaction IDs

- Start at 0x00000001 for first transaction in session
- Increment by 1 for each transaction
- Wrap from 0xFFFFFFFE to 0x00000001 (skip both 0xFFFFFFFF and 0x00000000)
- 0x00000000 used only for session-less operations (GetDeviceInfo before OpenSession)
- 0xFFFFFFFF is invalid and must never be used

## Sessions

- Most operations require an open session
- Session opened with `OpenSession` operation
- Session ID chosen by host (usually 1)
- Only `GetDeviceInfo` works without a session
- Most devices support only one session

## Operations

### GetDeviceInfo (0x1001)

**Parameters**: None
**Data phase**: R→I (DeviceInfo dataset)
**No session required**

### OpenSession (0x1002)

**Parameters**: Param1 = SessionID (usually 1)
**Data phase**: None

### CloseSession (0x1003)

**Parameters**: None
**Data phase**: None

### GetStorageIDs (0x1004)

**Parameters**: None
**Data phase**: R→I (array of u32 StorageIDs)

### GetStorageInfo (0x1005)

**Parameters**: Param1 = StorageID
**Data phase**: R→I (StorageInfo dataset)

### GetObjectHandles (0x1007)

**Parameters**:
- Param1: StorageID (0xFFFFFFFF = all storages)
- Param2: ObjectFormatCode (0x00000000 = all formats)
- Param3: Parent ObjectHandle:
  - 0x00000000 = objects in root folder only
  - 0xFFFFFFFF = all objects recursively
  - Other = objects in that specific folder

**Data phase**: R→I (array of u32 ObjectHandles)

### GetObjectInfo (0x1008)

**Parameters**: Param1 = ObjectHandle
**Data phase**: R→I (ObjectInfo dataset)

### GetObject (0x1009)

**Parameters**: Param1 = ObjectHandle
**Data phase**: R→I (object binary data)

### DeleteObject (0x100B)

**Parameters**:
- Param1: ObjectHandle
- Param2: ObjectFormatCode (0x00000000 to delete regardless of format)

**Data phase**: None

### SendObjectInfo (0x100C)

**Parameters**:
- Param1: StorageID (0xFFFFFFFF = let device choose)
- Param2: Parent ObjectHandle (0x00000000 = root folder)

**Data phase**: I→R (ObjectInfo dataset)

**Response parameters**:
- Param1: StorageID (assigned)
- Param2: Parent ObjectHandle (assigned)
- Param3: ObjectHandle (assigned to new object)

Must be followed by SendObject.

### SendObject (0x100D)

**Parameters**: None
**Data phase**: I→R (object binary data)

Must follow SendObjectInfo.

### MoveObject (0x1019)

**Parameters**:
- Param1: ObjectHandle
- Param2: StorageID (destination)
- Param3: Parent ObjectHandle (destination folder)

**Data phase**: None

### CopyObject (0x101A)

**Parameters**:
- Param1: ObjectHandle
- Param2: StorageID (destination)
- Param3: Parent ObjectHandle (destination folder)

**Response parameters**: Param1 = New ObjectHandle

## Response codes

| Code   | Name                    | Description                      |
|--------|-------------------------|----------------------------------|
| 0x2001 | OK                      | Success                          |
| 0x2002 | General_Error           | Unknown error                    |
| 0x2003 | Session_Not_Open        | Session required but not open    |
| 0x2005 | Operation_Not_Supported | Device doesn't support operation |
| 0x2008 | Invalid_StorageID       | Storage doesn't exist            |
| 0x2009 | Invalid_ObjectHandle    | Object doesn't exist             |
| 0x200C | Store_Full              | No space left                    |
| 0x200D | Object_WriteProtected   | Can't modify protected object    |
| 0x200F | Access_Denied           | Permission denied                |
| 0x2019 | Device_Busy             | Try again later                  |
| 0x201A | Invalid_ParentObject    | Parent isn't a folder            |

## Event codes

| Code   | Name               | Parameters   | Description             |
|--------|--------------------|--------------|-------------------------|
| 0x4002 | ObjectAdded        | ObjectHandle | New object created      |
| 0x4003 | ObjectRemoved      | ObjectHandle | Object deleted          |
| 0x4004 | StoreAdded         | StorageID    | Storage mounted         |
| 0x4005 | StoreRemoved       | StorageID    | Storage unmounted       |
| 0x4007 | ObjectInfoChanged  | ObjectHandle | Object metadata changed |
| 0x4008 | DeviceInfoChanged  | (none)       | Device info changed     |
| 0x400C | StorageInfoChanged | StorageID    | Storage info changed    |

## Data structures

### DeviceInfo dataset

| Field                     | Type       | Description                                |
|---------------------------|------------|--------------------------------------------|
| StandardVersion           | u16        | PTP version × 100 (100 = 1.00)             |
| VendorExtensionID         | u32        | 0xFFFFFFFF for MTP                         |
| VendorExtensionVersion    | u16        | MTP version × 100                          |
| VendorExtensionDesc       | String     | Extension info (e.g., "android.com: 1.0;") |
| FunctionalMode            | u16        | 0x0000 = standard mode                     |
| OperationsSupported       | Array<u16> | Supported operation codes                  |
| EventsSupported           | Array<u16> | Supported event codes                      |
| DevicePropertiesSupported | Array<u16> | Supported device property codes            |
| CaptureFormats            | Array<u16> | Formats device can create                  |
| PlaybackFormats           | Array<u16> | Formats device can read                    |
| Manufacturer              | String     | Device manufacturer                        |
| Model                     | String     | Device model                               |
| DeviceVersion             | String     | Firmware version                           |
| SerialNumber              | String     | Unique identifier                          |

### StorageInfo dataset

| Field              | Type   | Description                                |
|--------------------|--------|--------------------------------------------|
| StorageType        | u16    | 0x0003 = Fixed RAM, 0x0004 = Removable RAM |
| FilesystemType     | u16    | 0x0002 = Generic hierarchical              |
| AccessCapability   | u16    | 0x0000 = Read-write                        |
| MaxCapacity        | u64    | Total capacity in bytes                    |
| FreeSpaceInBytes   | u64    | Available space                            |
| FreeSpaceInObjects | u32    | 0xFFFFFFFF if not applicable               |
| StorageDescription | String | Human-readable name                        |
| VolumeIdentifier   | String | Volume serial number                       |

### ObjectInfo dataset

| Field                | Type   | Required for Send | Description                    |
|----------------------|--------|-------------------|--------------------------------|
| StorageID            | u32    | No                | Storage containing object      |
| ObjectFormat         | u16    | Yes               | Object type code               |
| ProtectionStatus     | u16    | No                | Write protection               |
| ObjectCompressedSize | u32    | Yes               | File size (0xFFFFFFFF if >4GB) |
| ThumbFormat          | u16    | No                | Thumbnail format               |
| ThumbCompressedSize  | u32    | No                | Thumbnail size                 |
| ThumbPixWidth        | u32    | No                | Thumbnail width                |
| ThumbPixHeight       | u32    | No                | Thumbnail height               |
| ImagePixWidth        | u32    | No                | Image width                    |
| ImagePixHeight       | u32    | No                | Image height                   |
| ImageBitDepth        | u32    | No                | Bits per pixel                 |
| ParentObject         | u32    | No                | Parent folder handle           |
| AssociationType      | u16    | Yes               | 0x0001 for folders             |
| AssociationDesc      | u32    | Yes               | 0x00000000 normally            |
| SequenceNumber       | u32    | No                | Unused                         |
| Filename             | String | Yes               | Object name                    |
| DateCreated          | String | No                | See datetime format below      |
| DateModified         | String | No                | See datetime format below      |
| Keywords             | String | No                | Unused                         |

Datetime format: `YYYYMMDDThhmmss`, then optional tenths of a second (`.s`), then an optional UTC
offset (`Z` or `±hhmm`). An empty string means no date. Without an offset the value is local time in
a zone the string doesn't name; Android always writes it that way, in the phone's zone. `DateTime`
keeps the offset in `offset` (`None` when absent) and drops the tenths.

## Object format codes

| Code   | Format      | Description         |
|--------|-------------|---------------------|
| 0x3000 | Undefined   | Unknown binary      |
| 0x3001 | Association | Folder              |
| 0x3004 | Text        | Plain text          |
| 0x3008 | WAV         | WAV audio           |
| 0x3009 | MP3         | MP3 audio           |
| 0x300B | MPEG        | MPEG video          |
| 0x3801 | JPEG        | JPEG image          |
| 0x380B | PNG         | PNG image           |
| 0xB903 | AAC         | AAC audio           |
| 0xB982 | MP4         | MP4 container       |

## USB endpoints

MTP devices expose three endpoints:

| Endpoint     | Type      | Direction     | Purpose                        |
|--------------|-----------|---------------|--------------------------------|
| Bulk OUT     | Bulk      | Host → Device | Commands and data to device    |
| Bulk IN      | Bulk      | Device → Host | Responses and data from device |
| Interrupt IN | Interrupt | Device → Host | Events                         |

Typical max packet sizes:
- USB 2.0 High Speed: 512 bytes
- USB 3.0 Super Speed: 1024 bytes

## Android-specific behavior

All modern Android devices:

- Report "android.com" in VendorExtensionDesc
- Support hierarchical filesystem (AssociationType = 0x0001)
- May ignore recursive listing requests (ObjectHandle::ALL returns incomplete results)
- Need 30+ second timeouts for large transfers
- Don't allow creating files/folders in storage root

See `README.md` for how mtp-rs handles these automatically.
