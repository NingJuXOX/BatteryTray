use crate::battery::{HidBackend, HidDevice, HidInterface, HidLock, OpenOptions};
use crate::error::{BatteryTrayError, Result};
use crate::platform::sync::{hid_lock_name, NamedMutex};
use std::mem::{size_of, zeroed};
use windows::core::{GUID, HRESULT, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetFeature, HidD_GetHidGuid,
    HidD_GetManufacturerString, HidD_GetPreparsedData, HidD_GetProductString,
    HidD_GetSerialNumberString, HidD_SetFeature, HidP_GetCaps, HIDD_ATTRIBUTES, HIDP_CAPS,
    PHIDP_PREPARSED_DATA,
};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_IO_PENDING, ERROR_NOT_FOUND, ERROR_NO_MORE_ITEMS,
    ERROR_OPERATION_ABORTED, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, NTSTATUS,
    WAIT_OBJECT_0,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_OVERLAPPED,
    FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

const HIDP_STATUS_SUCCESS: NTSTATUS = NTSTATUS(0x0011_0000);
const HID_LOCK_TIMEOUT_MS: u32 = 8000;
/// Wait granted to the driver after a cancelled read so its buffers can be
/// reclaimed safely. Until this expires the operation may still be in flight.
const CANCEL_GRACE_MS: u32 = 2_000;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn hid_error(operation: &str) -> BatteryTrayError {
    BatteryTrayError::Hid(format!("{operation}：{}", std::io::Error::last_os_error()))
}

fn close_handle(handle: HANDLE) {
    if !handle.is_invalid() {
        // SAFETY: callers only pass handles they own; failure is non-fatal cleanup.
        if let Err(error) = unsafe { CloseHandle(handle) } {
            eprintln!("关闭 HID 句柄失败：{error}");
        }
    }
}

/// Windows SetupAPI/HID backend.
#[derive(Debug, Default)]
pub struct WindowsHidBackend;

impl WindowsHidBackend {
    /// Creates a stateless Windows backend.
    pub fn new() -> Self {
        Self
    }
}

impl HidBackend for WindowsHidBackend {
    fn enumerate(&mut self) -> Result<Vec<HidInterface>> {
        enumerate_hid()
    }

    fn open(
        &mut self,
        interface: &HidInterface,
        options: OpenOptions,
    ) -> Result<Box<dyn HidDevice>> {
        WindowsHidDevice::open(interface, options)
            .map(|device| Box::new(device) as Box<dyn HidDevice>)
    }

    fn lock(&mut self, key: &str, timeout_ms: u32) -> Result<Option<Box<dyn HidLock>>> {
        let timeout = if timeout_ms == 0 {
            HID_LOCK_TIMEOUT_MS
        } else {
            timeout_ms
        };
        NamedMutex::acquire(&hid_lock_name(key), timeout)
            .map(|lock| lock.map(|item| Box::new(item) as Box<dyn HidLock>))
    }
}

struct DeviceInfoSet(windows::Win32::Devices::DeviceAndDriverInstallation::HDEVINFO);

impl Drop for DeviceInfoSet {
    fn drop(&mut self) {
        // SAFETY: the set was returned by SetupDiGetClassDevsW and is destroyed once.
        if let Err(error) = unsafe { SetupDiDestroyDeviceInfoList(self.0) } {
            eprintln!("销毁 HID 枚举集合失败：{error}");
        }
    }
}

/// Enumerates present HID interfaces without requesting write access.
pub fn enumerate_hid() -> Result<Vec<HidInterface>> {
    // SAFETY: HidD_GetHidGuid has no parameters and returns a value-type GUID.
    let guid: GUID = unsafe { HidD_GetHidGuid() };
    // SAFETY: GUID remains alive for the call and no enumerator/window is used.
    let info = unsafe {
        SetupDiGetClassDevsW(
            Some(&guid),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    }
    .map_err(|error| BatteryTrayError::Hid(format!("SetupDiGetClassDevsW 失败：{error}")))?;
    if info.is_invalid() {
        return Err(hid_error("SetupDiGetClassDevsW 返回无效句柄"));
    }
    let info = DeviceInfoSet(info);
    let mut result = Vec::new();
    for index in 0_u32..4096 {
        // SAFETY: zero is a valid initial state for this POD SetupAPI structure.
        let mut data: SP_DEVICE_INTERFACE_DATA = unsafe { zeroed() };
        data.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
        // SAFETY: `data` is initialized with its required size and remains live.
        match unsafe { SetupDiEnumDeviceInterfaces(info.0, None, &guid, index, &mut data) } {
            Ok(()) => {}
            // Use the error returned by SetupAPI rather than GetLastError, which
            // any intermediate call could have overwritten.
            Err(error) if error.code() == HRESULT::from_win32(ERROR_NO_MORE_ITEMS.0) => break,
            Err(error) => {
                return Err(BatteryTrayError::Hid(format!(
                    "SetupDiEnumDeviceInterfaces 失败：{error}"
                )))
            }
        }
        let mut required = 0_u32;
        // SAFETY: the first sizing call intentionally passes no detail buffer.
        if let Err(error) = unsafe {
            SetupDiGetDeviceInterfaceDetailW(info.0, &data, None, 0, Some(&mut required), None)
        } {
            let _ = error;
        }
        if required < 8 {
            continue;
        }
        let mut raw = vec![0_u8; required as usize];
        let detail = raw.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        // SAFETY: `detail` points to `required` writable bytes. cbSize is the
        // documented fixed prefix size (8 on x64, 6 on x86).
        unsafe {
            (*detail).cbSize = if size_of::<usize>() == 8 { 8 } else { 6 };
        }
        // SAFETY: all pointers reference live buffers of the sizes supplied.
        if unsafe {
            SetupDiGetDeviceInterfaceDetailW(info.0, &data, Some(detail), required, None, None)
        }
        .is_err()
        {
            continue;
        }
        // SAFETY: SetupAPI returned a NUL-terminated path within `raw`.
        let path_ptr = unsafe { std::ptr::addr_of!((*detail).DevicePath).cast::<u16>() };
        let max_chars = (required as usize).saturating_sub(4) / 2;
        let mut length = 0_usize;
        while length < max_chars {
            // SAFETY: the scan is bounded by the allocated detail buffer.
            if unsafe { *path_ptr.add(length) } == 0 {
                break;
            }
            length += 1;
        }
        // SAFETY: `length` UTF-16 units were validated inside `raw`.
        let path =
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(path_ptr, length) });
        match inspect_interface(&path) {
            Ok(Some(interface)) => result.push(interface),
            Ok(None) => {}
            Err(error) => eprintln!("读取 HID 接口信息失败：{error}"),
        }
    }
    Ok(result)
}

fn open_raw(path: &str, access: u32, flags: FILE_FLAGS_AND_ATTRIBUTES) -> Result<HANDLE> {
    let path = wide(path);
    // SAFETY: path is NUL terminated and optional security/template pointers are null.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            access,
            FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0),
            None,
            OPEN_EXISTING,
            flags,
            None,
        )
    }
    .map_err(|error| BatteryTrayError::Hid(format!("CreateFileW 失败：{error}")))?;
    if handle == INVALID_HANDLE_VALUE || handle.is_invalid() {
        return Err(hid_error("CreateFileW 失败"));
    }
    Ok(handle)
}

fn device_string(handle: HANDLE, which: &str) -> String {
    let mut buffer = [0_u16; 256];
    let bytes = (buffer.len() * size_of::<u16>()) as u32;
    // SAFETY: buffer is writable for `bytes`; each HidD string function has the
    // same handle/buffer/size contract and does not retain the pointer.
    let ok = unsafe {
        match which {
            "product" => HidD_GetProductString(handle, buffer.as_mut_ptr().cast(), bytes),
            "manufacturer" => HidD_GetManufacturerString(handle, buffer.as_mut_ptr().cast(), bytes),
            "serial" => HidD_GetSerialNumberString(handle, buffer.as_mut_ptr().cast(), bytes),
            _ => return String::new(),
        }
    };
    if !ok.as_bool() {
        return String::new();
    }
    let length = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
}

fn interface_number(path: &str) -> i16 {
    let lower = path.to_lowercase();
    let Some(position) = lower.find("&mi_") else {
        return -1;
    };
    lower
        .get(position + 4..position + 6)
        .and_then(|value| i16::from_str_radix(value, 16).ok())
        .unwrap_or(-1)
}

fn inspect_interface(path: &str) -> Result<Option<HidInterface>> {
    let handle = match open_raw(path, 0, FILE_FLAGS_AND_ATTRIBUTES(0)) {
        Ok(handle) => handle,
        Err(_) => return Ok(None),
    };
    let result = (|| -> Result<Option<HidInterface>> {
        // SAFETY: zeroed attributes with Size initialized is required by HID.
        let mut attributes: HIDD_ATTRIBUTES = unsafe { zeroed() };
        attributes.Size = size_of::<HIDD_ATTRIBUTES>() as u32;
        // SAFETY: handle and attributes are valid for this call.
        if !unsafe { HidD_GetAttributes(handle, &mut attributes) }.as_bool() {
            return Ok(None);
        }
        let mut preparsed = PHIDP_PREPARSED_DATA(0);
        // SAFETY: preparsed points to writable storage of the required type.
        if !unsafe { HidD_GetPreparsedData(handle, &mut preparsed) }.as_bool() {
            return Ok(None);
        }
        // SAFETY: zero is the documented initial value for HIDP_CAPS.
        let mut caps: HIDP_CAPS = unsafe { zeroed() };
        // SAFETY: preparsed and caps are valid values; HidP does not retain caps.
        let status = unsafe { HidP_GetCaps(preparsed, &mut caps) };
        // SAFETY: preparsed was allocated by HidD and is freed exactly once.
        if !unsafe { HidD_FreePreparsedData(preparsed) }.as_bool() {
            return Err(hid_error("HidD_FreePreparsedData 失败"));
        }
        if status != HIDP_STATUS_SUCCESS {
            return Ok(None);
        }
        Ok(Some(HidInterface {
            path: path.to_owned(),
            vendor_id: attributes.VendorID,
            product_id: attributes.ProductID,
            product: device_string(handle, "product"),
            manufacturer: device_string(handle, "manufacturer"),
            serial: device_string(handle, "serial"),
            usage_page: caps.UsagePage,
            usage: caps.Usage,
            input_len: caps.InputReportByteLength,
            output_len: caps.OutputReportByteLength,
            feature_len: caps.FeatureReportByteLength,
            interface_number: interface_number(path),
        }))
    })();
    close_handle(handle);
    result
}

/// One overlapped read whose completion may outlive the call that started it.
///
/// `OVERLAPPED` and the report buffer must stay at a stable address until the
/// kernel signals completion. Keeping them on the heap lets the device retain
/// ownership after a timeout instead of freeing stack memory the driver may
/// still write to.
struct PendingRead {
    overlapped: Box<OVERLAPPED>,
    buffer: Vec<u8>,
}

impl PendingRead {
    fn new(event: HANDLE, size: usize) -> Self {
        // SAFETY: OVERLAPPED is a POD structure where all-zero means "no file
        // offset"; the event is assigned immediately afterwards.
        let mut overlapped: Box<OVERLAPPED> = Box::new(unsafe { zeroed() });
        overlapped.hEvent = event;
        Self {
            overlapped,
            buffer: vec![0_u8; size],
        }
    }
}

/// Result of checking a pending overlapped read without blocking.
enum ReapOutcome {
    /// The operation finished and its buffers can be released.
    Completed,
    /// The driver still owns the buffers; they must not be freed.
    Pending(PendingRead),
}

struct WindowsHidDevice {
    interface: HidInterface,
    write_handle: HANDLE,
    read_handle: HANDLE,
    event: HANDLE,
    last_error: u32,
    pending: Option<PendingRead>,
}

// SAFETY: the device is used by one protocol worker at a time. Kernel handles
// can be moved to that worker; methods still require `&mut self`.
unsafe impl Send for WindowsHidDevice {}

impl WindowsHidDevice {
    fn open(interface: &HidInterface, options: OpenOptions) -> Result<Self> {
        let access = if options.write {
            GENERIC_READ.0 | GENERIC_WRITE.0
        } else {
            0
        };
        let write_handle = open_raw(&interface.path, access, FILE_FLAGS_AND_ATTRIBUTES(0))?;
        let mut device = Self {
            interface: interface.clone(),
            write_handle,
            read_handle: HANDLE::default(),
            event: HANDLE::default(),
            last_error: 0,
            pending: None,
        };
        if options.read {
            // On failure `device` is dropped and its Drop impl closes every
            // handle exactly once; closing manually would double-close.
            device.read_handle = open_raw(&interface.path, GENERIC_READ.0, FILE_FLAG_OVERLAPPED)?;
            // SAFETY: no security attributes or name; manual reset is required.
            let event = unsafe { CreateEventW(None, true, false, None) }
                .map_err(|error| BatteryTrayError::Hid(format!("CreateEventW 失败：{error}")))?;
            device.event = event;
        }
        Ok(device)
    }

    fn remember_error(&mut self) {
        // SAFETY: called immediately after the failed Win32 operation.
        self.last_error = unsafe { GetLastError() }.0;
    }

    /// Checks a previously abandoned read without blocking.
    fn reap(&self, pending: PendingRead) -> ReapOutcome {
        // SAFETY: the event belongs to this device and the timeout is zero.
        let status = unsafe { WaitForSingleObject(self.event, 0) };
        if status != WAIT_OBJECT_0 {
            return ReapOutcome::Pending(pending);
        }
        let mut read = 0_u32;
        // SAFETY: the overlapped structure and counter stay alive for the call;
        // `false` keeps this non-blocking because the event is signalled.
        if unsafe { GetOverlappedResult(self.read_handle, &*pending.overlapped, &mut read, false) }
            .is_err()
        {
            return ReapOutcome::Completed;
        }
        ReapOutcome::Completed
    }

    /// Cancels an in-flight read and gives the driver a bounded grace period.
    ///
    /// Returns the buffers when the kernel has not completed the operation yet;
    /// the caller must keep them alive instead of freeing them.
    fn abandon(&self, pending: PendingRead) -> Option<PendingRead> {
        // SAFETY: cancels exactly the operation identified by this OVERLAPPED.
        match unsafe { CancelIoEx(self.read_handle, Some(&*pending.overlapped)) } {
            Ok(()) => {}
            // The operation already completed before the cancel arrived.
            Err(error) if error.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0) => {}
            Err(error) => eprintln!("CancelIoEx 失败：{error}"),
        }
        // SAFETY: bounded wait for the cancellation to be observed.
        if unsafe { WaitForSingleObject(self.event, CANCEL_GRACE_MS) } == WAIT_OBJECT_0 {
            let mut read = 0_u32;
            // SAFETY: drains the completion so the buffers can be dropped.
            // A cancelled operation reports ERROR_OPERATION_ABORTED, which is
            // the expected outcome here rather than a failure.
            match unsafe {
                GetOverlappedResult(self.read_handle, &*pending.overlapped, &mut read, false)
            } {
                Ok(()) => {}
                Err(error) if error.code() == HRESULT::from_win32(ERROR_OPERATION_ABORTED.0) => {}
                Err(error) => eprintln!("清理已取消的 HID 读请求失败：{error}"),
            }
            None
        } else {
            Some(pending)
        }
    }
}

impl Drop for WindowsHidDevice {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            if !self.read_handle.is_invalid() && !self.event.is_invalid() {
                // SAFETY: cancels only the operation this device started.
                if let Err(error) =
                    // SAFETY: cancels only the operation this device started.
                    unsafe { CancelIoEx(self.read_handle, Some(&*pending.overlapped)) }
                {
                    eprintln!("关闭时取消 HID 读请求失败：{error}");
                }
                // SAFETY: bounded wait so shutdown cannot hang on a stuck driver.
                let status = unsafe { WaitForSingleObject(self.event, CANCEL_GRACE_MS) };
                if status != WAIT_OBJECT_0 {
                    // The kernel may still write into these buffers. Leaking
                    // them is the only alternative to a use-after-free.
                    eprintln!("HID 读请求在关闭时仍未完成，保留缓冲区以避免悬垂写入");
                    let _leaked = Box::leak(Box::new(pending));
                }
            }
        }
        close_handle(self.event);
        close_handle(self.read_handle);
        close_handle(self.write_handle);
    }
}

impl HidDevice for WindowsHidDevice {
    fn set_feature(&mut self, data: &[u8]) -> Result<bool> {
        let size = if self.interface.feature_len > 0 {
            usize::from(self.interface.feature_len)
        } else {
            data.len()
        };
        if data.len() > size {
            return Ok(false);
        }
        let mut buffer = vec![0_u8; size];
        buffer[..data.len()].copy_from_slice(data);
        // SAFETY: buffer is writable for exactly `size` bytes and is not retained.
        let ok =
            unsafe { HidD_SetFeature(self.write_handle, buffer.as_mut_ptr().cast(), size as u32) };
        if !ok.as_bool() {
            self.remember_error();
        }
        Ok(ok.as_bool())
    }

    fn get_feature(&mut self, length: Option<usize>, report_id: u8) -> Result<Option<Vec<u8>>> {
        let size = length
            .filter(|value| *value > 0)
            .unwrap_or(usize::from(self.interface.feature_len));
        if size == 0 {
            return Ok(None);
        }
        let mut buffer = vec![0_u8; size];
        buffer[0] = report_id;
        // SAFETY: buffer has `size` writable bytes and report ID is initialized.
        let ok =
            unsafe { HidD_GetFeature(self.write_handle, buffer.as_mut_ptr().cast(), size as u32) };
        if !ok.as_bool() {
            self.remember_error();
            return Ok(None);
        }
        Ok(Some(buffer))
    }

    fn write_output(&mut self, data: &[u8]) -> Result<bool> {
        let size = if self.interface.output_len > 0 {
            usize::from(self.interface.output_len)
        } else {
            data.len()
        };
        if data.len() > size {
            return Ok(false);
        }
        let mut buffer = vec![0_u8; size];
        buffer[..data.len()].copy_from_slice(data);
        let mut written = 0_u32;
        // SAFETY: buffer is readable for `size` bytes; synchronous handle means
        // no OVERLAPPED pointer is required.
        let result =
            unsafe { WriteFile(self.write_handle, Some(&buffer), Some(&mut written), None) };
        if result.is_err() {
            self.remember_error();
            return Ok(false);
        }
        Ok(true)
    }

    fn read_input(&mut self, timeout_ms: u32) -> Result<Option<Vec<u8>>> {
        if self.read_handle.is_invalid() || self.event.is_invalid() {
            return Ok(None);
        }
        // A timed-out read may still be owned by the driver. Starting a second
        // read on the same handle would give it two outstanding operations, so
        // wait for the abandoned one to finish first.
        if let Some(pending) = self.pending.take() {
            match self.reap(pending) {
                ReapOutcome::Pending(pending) => {
                    self.pending = Some(pending);
                    self.last_error = 258;
                    return Ok(None);
                }
                // Data from an earlier request is stale; discard and re-read.
                ReapOutcome::Completed => {}
            }
        }
        let size = if self.interface.input_len > 0 {
            usize::from(self.interface.input_len)
        } else {
            64
        };
        let mut pending = PendingRead::new(self.event, size);
        let mut read = 0_u32;
        // SAFETY: resets the valid event before starting a new overlapped read.
        if let Err(error) = unsafe { ResetEvent(self.event) } {
            return Err(BatteryTrayError::Hid(format!("ResetEvent 失败：{error}")));
        }
        // SAFETY: the buffers are owned by `pending` and stay at a stable heap
        // address for as long as the operation may be in flight.
        let immediate = unsafe {
            ReadFile(
                self.read_handle,
                Some(&mut pending.buffer),
                Some(&mut read),
                Some(&mut *pending.overlapped),
            )
        };
        if immediate.is_ok() {
            let mut buffer = pending.buffer;
            buffer.truncate(read as usize);
            return Ok(Some(buffer));
        }
        // SAFETY: read this thread's error immediately after ReadFile.
        let error = unsafe { GetLastError() };
        if error != ERROR_IO_PENDING {
            self.last_error = error.0;
            return Ok(None);
        }
        // SAFETY: waits on the operation's event with the caller's timeout.
        let status = unsafe { WaitForSingleObject(self.event, timeout_ms) };
        if status == WAIT_OBJECT_0 {
            let mut count = 0_u32;
            // SAFETY: the overlapped structure and counter are still valid.
            if unsafe {
                GetOverlappedResult(self.read_handle, &*pending.overlapped, &mut count, false)
            }
            .is_err()
            {
                self.remember_error();
                return Ok(None);
            }
            let mut buffer = pending.buffer;
            buffer.truncate(count as usize);
            return Ok(Some(buffer));
        }
        // Timeout or wait failure: cancel, then keep the buffers if the driver
        // has not completed yet. They stay reachable through `self.pending`.
        if let Some(pending) = self.abandon(pending) {
            self.pending = Some(pending);
        }
        self.last_error = 258;
        Ok(None)
    }

    fn last_error(&self) -> u32 {
        self.last_error
    }
}
