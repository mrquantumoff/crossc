//! The SPA handle factory the codec loader opens, and the exported log topic list.

use core::ffi::{CStr, c_char, c_int, c_void};
use core::ptr;

use crate::codec::CODEC;
use crate::log::TOPIC;
use crate::slot;
use crate::sys;

const EINVAL: c_int = 22;
const ENOENT: c_int = 2;

#[repr(transparent)]
pub struct Shared<T>(T);
unsafe impl<T> Sync for Shared<T> {}

static CODECS: Shared<[*const sys::media_codec; 2]> = Shared([&CODEC.0, ptr::null()]);

#[repr(C)]
struct Handle {
    handle: sys::spa_handle,
    a2dp: sys::spa_bluez5_codec_a2dp,
}

unsafe extern "C" fn get_interface(
    handle: *mut sys::spa_handle,
    type_: *const c_char,
    iface: *mut *mut c_void,
) -> c_int {
    if handle.is_null() || type_.is_null() || iface.is_null() {
        return -EINVAL;
    }
    if unsafe { CStr::from_ptr(type_) } != sys::SPA_TYPE_INTERFACE_Bluez5CodecMedia {
        return -ENOENT;
    }
    let this = handle as *mut Handle;
    unsafe { *iface = &raw mut (*this).a2dp as *mut c_void };
    0
}

unsafe extern "C" fn clear(handle: *mut sys::spa_handle) -> c_int {
    if handle.is_null() { -EINVAL } else { 0 }
}

unsafe extern "C" fn get_size(
    _factory: *const sys::spa_handle_factory,
    _params: *const sys::spa_dict,
) -> usize {
    size_of::<Handle>()
}

unsafe extern "C" fn init(
    factory: *const sys::spa_handle_factory,
    handle: *mut sys::spa_handle,
    _info: *const sys::spa_dict,
    _support: *const sys::spa_support,
    _n_support: u32,
) -> c_int {
    if factory.is_null() || handle.is_null() {
        return -EINVAL;
    }
    let this = handle as *mut Handle;
    unsafe {
        (*this).handle.get_interface = Some(get_interface);
        (*this).handle.clear = Some(clear);
        (*this).a2dp = sys::spa_bluez5_codec_a2dp {
            iface: sys::spa_interface {
                type_: sys::SPA_TYPE_INTERFACE_Bluez5CodecMedia.as_ptr(),
                version: sys::SPA_VERSION_BLUEZ5_CODEC_MEDIA,
                cb: sys::spa_callbacks {
                    funcs: ptr::null(),
                    data: this as *mut c_void,
                },
            },
            codecs: CODECS.0.as_ptr(),
        };
    }
    0
}

static INTERFACES: Shared<[sys::spa_interface_info; 1]> = Shared([sys::spa_interface_info {
    type_: sys::SPA_TYPE_INTERFACE_Bluez5CodecMedia.as_ptr(),
}]);

unsafe extern "C" fn enum_interface_info(
    factory: *const sys::spa_handle_factory,
    info: *mut *const sys::spa_interface_info,
    index: *mut u32,
) -> c_int {
    if factory.is_null() || info.is_null() || index.is_null() {
        return -EINVAL;
    }
    unsafe {
        let Some(i) = INTERFACES.0.get(*index as usize) else {
            return 0;
        };
        *info = i;
        *index += 1;
    }
    1
}

static INFO_ITEMS: Shared<[sys::spa_dict_item; 1]> = Shared([sys::spa_dict_item {
    key: c"factory.description".as_ptr(),
    value: c"Bluetooth codec plugin: Samsung SSC".as_ptr(),
}]);

static INFO: Shared<sys::spa_dict> = Shared(sys::spa_dict {
    flags: 0,
    n_items: 1,
    items: &INFO_ITEMS.0 as *const _ as *const sys::spa_dict_item,
});

static FACTORY: Shared<sys::spa_handle_factory> = Shared(sys::spa_handle_factory {
    version: sys::SPA_VERSION_HANDLE_FACTORY,
    name: slot::FACTORY_NAME.as_ptr(),
    info: &INFO.0,
    get_size: Some(get_size),
    init: Some(init),
    enum_interface_info: Some(enum_interface_info),
});

/// # Safety
/// Called by the SPA plugin loader with valid out pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn spa_handle_factory_enum(
    factory: *mut *const sys::spa_handle_factory,
    index: *mut u32,
) -> c_int {
    if factory.is_null() || index.is_null() {
        return -EINVAL;
    }
    unsafe {
        if *index != 0 {
            return 0;
        }
        *factory = &FACTORY.0;
        *index += 1;
    }
    1
}

static TOPICS: Shared<[*mut sys::spa_log_topic; 1]> = Shared([TOPIC.get()]);

#[allow(non_upper_case_globals)]
#[unsafe(no_mangle)]
pub static spa_log_topic_enum: Shared<sys::spa_log_topic_enum> = Shared(sys::spa_log_topic_enum {
    version: sys::SPA_VERSION_LOG_TOPIC_ENUM,
    topics: TOPICS.0.as_ptr(),
    topics_end: TOPICS.0.as_ptr().wrapping_add(1),
});
