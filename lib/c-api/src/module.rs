//! Compile, validate, instantiate, serialize, and destroy modules.

use crate::{
    error::{update_last_error, CApiError},
    export::wasmer_import_export_kind,
    import::{wasmer_import_object_t, wasmer_import_t, CAPIImportObject},
    instance::{wasmer_instance_t, CAPIInstance},
    wasmer_byte_array, wasmer_result_t,
};
use libc::c_int;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::slice;
use wasmer::{Exports, Extern, Function, Global, ImportObject, Instance, Memory, Module, Table};

#[repr(C)]
pub struct wasmer_module_t;

#[repr(C)]
pub struct wasmer_serialized_module_t;

/// Creates a new Module from the given wasm bytes.
///
/// Returns `wasmer_result_t::WASMER_OK` upon success.
///
/// Returns `wasmer_result_t::WASMER_ERROR` upon failure. Use `wasmer_last_error_length`
/// and `wasmer_last_error_message` to get an error message.
#[allow(clippy::cast_ptr_alignment)]
#[no_mangle]
pub unsafe extern "C" fn wasmer_compile(
    module: *mut *mut wasmer_module_t,
    wasm_bytes: *mut u8,
    wasm_bytes_len: u32,
) -> wasmer_result_t {
    let bytes: &[u8] = slice::from_raw_parts_mut(wasm_bytes, wasm_bytes_len as usize);
    let store = crate::get_global_store();
    let result = Module::from_binary(store, bytes);
    let new_module = match result {
        Ok(instance) => instance,
        Err(error) => {
            update_last_error(error);
            return wasmer_result_t::WASMER_ERROR;
        }
    };
    *module = Box::into_raw(Box::new(new_module)) as *mut wasmer_module_t;
    wasmer_result_t::WASMER_OK
}

/// Validates a sequence of bytes hoping it represents a valid WebAssembly module.
///
/// The function returns true if the bytes are valid, false otherwise.
///
/// Example:
///
/// ```c
/// bool result = wasmer_validate(bytes, bytes_length);
///
/// if (false == result) {
///     // Do something…
/// }
/// ```
#[allow(clippy::cast_ptr_alignment)]
#[no_mangle]
pub unsafe extern "C" fn wasmer_validate(wasm_bytes: *const u8, wasm_bytes_len: u32) -> bool {
    if wasm_bytes.is_null() {
        return false;
    }

    let bytes: &[u8] = slice::from_raw_parts(wasm_bytes, wasm_bytes_len as usize);

    let store = crate::get_global_store();
    Module::validate(store, bytes).is_ok()
}

/// Creates a new Instance from the given module and imports.
///
/// Returns `wasmer_result_t::WASMER_OK` upon success.
///
/// Returns `wasmer_result_t::WASMER_ERROR` upon failure. Use `wasmer_last_error_length`
/// and `wasmer_last_error_message` to get an error message.
#[allow(clippy::cast_ptr_alignment)]
#[no_mangle]
pub unsafe extern "C" fn wasmer_module_instantiate(
    module: *const wasmer_module_t,
    instance: *mut *mut wasmer_instance_t,
    imports: *mut wasmer_import_t,
    imports_len: c_int,
) -> wasmer_result_t {
    let imports: &[wasmer_import_t] = slice::from_raw_parts(imports, imports_len as usize);
    let mut imported_memories = vec![];
    let mut import_object = ImportObject::new();
    let mut namespaces = HashMap::new();
    for import in imports {
        let module_name = slice::from_raw_parts(
            import.module_name.bytes,
            import.module_name.bytes_len as usize,
        );
        let module_name = if let Ok(s) = std::str::from_utf8(module_name) {
            s
        } else {
            update_last_error(CApiError {
                msg: "error converting module name to string".to_string(),
            });
            return wasmer_result_t::WASMER_ERROR;
        };
        let import_name = slice::from_raw_parts(
            import.import_name.bytes,
            import.import_name.bytes_len as usize,
        );
        let import_name = if let Ok(s) = std::str::from_utf8(import_name) {
            s
        } else {
            update_last_error(CApiError {
                msg: "error converting import_name to string".to_string(),
            });
            return wasmer_result_t::WASMER_ERROR;
        };

        let namespace = namespaces.entry(module_name).or_insert_with(Exports::new);

        let export = match import.tag {
            wasmer_import_export_kind::WASM_MEMORY => {
                let mem = import.value.memory as *mut Memory;
                imported_memories.push(mem);
                Extern::Memory((&*mem).clone())
            }
            wasmer_import_export_kind::WASM_FUNCTION => {
                let func_export = import.value.func as *mut Function;
                Extern::Function((&*func_export).clone())
            }
            wasmer_import_export_kind::WASM_GLOBAL => {
                let global = import.value.global as *mut Global;
                Extern::Global((&*global).clone())
            }
            wasmer_import_export_kind::WASM_TABLE => {
                let table = import.value.table as *mut Table;
                Extern::Table((&*table).clone())
            }
        };
        namespace.insert(import_name, export);
    }
    for (module_name, namespace) in namespaces.into_iter() {
        import_object.register(module_name, namespace);
    }

    let module = &*(module as *const Module);
    let new_instance = match Instance::new(module, &import_object) {
        Ok(instance) => instance,
        Err(error) => {
            update_last_error(error);
            return wasmer_result_t::WASMER_ERROR;
        }
    };

    let c_api_instance = CAPIInstance {
        instance: new_instance,
        imported_memories,
        ctx_data: None,
    };

    *instance = Box::into_raw(Box::new(c_api_instance)) as *mut wasmer_instance_t;
    wasmer_result_t::WASMER_OK
}

/// Given:
/// * A prepared `wasmer` import-object
/// * A compiled wasmer module
///
/// Instantiates a wasmer instance
#[no_mangle]
pub unsafe extern "C" fn wasmer_module_import_instantiate(
    instance: *mut *mut wasmer_instance_t,
    module: *const wasmer_module_t,
    import_object: *const wasmer_import_object_t,
) -> wasmer_result_t {
    // mutable to mutate through `instance_pointers_to_update` to make host functions work
    let import_object: &mut CAPIImportObject = &mut *(import_object as *mut CAPIImportObject);
    let module: &Module = &*(module as *const Module);

    let new_instance: Instance = match Instance::new(module, &import_object.import_object) {
        Ok(instance) => instance,
        Err(error) => {
            update_last_error(error);
            return wasmer_result_t::WASMER_ERROR;
        }
    };
    let c_api_instance = CAPIInstance {
        instance: new_instance,
        imported_memories: import_object.imported_memories.clone(),
        ctx_data: None,
    };
    let c_api_instance_pointer = Box::into_raw(Box::new(c_api_instance));
    for to_update in import_object.instance_pointers_to_update.iter_mut() {
        to_update.as_mut().instance_ptr = Some(NonNull::new_unchecked(c_api_instance_pointer));
    }
    *instance = c_api_instance_pointer as *mut wasmer_instance_t;

    return wasmer_result_t::WASMER_OK;
}

/// Serialize the given Module.
///
/// The caller owns the object and should call `wasmer_serialized_module_destroy` to free it.
///
/// Returns `wasmer_result_t::WASMER_OK` upon success.
///
/// Returns `wasmer_result_t::WASMER_ERROR` upon failure. Use `wasmer_last_error_length`
/// and `wasmer_last_error_message` to get an error message.
#[allow(clippy::cast_ptr_alignment)]
#[no_mangle]
pub unsafe extern "C" fn wasmer_module_serialize(
    serialized_module_out: *mut *mut wasmer_serialized_module_t,
    module: *const wasmer_module_t,
) -> wasmer_result_t {
    let module = &*(module as *const Module);

    match module.serialize() {
        Ok(serialized_module) => {
            let boxed_slice = serialized_module.into_boxed_slice();
            *serialized_module_out = Box::into_raw(Box::new(boxed_slice)) as _;

            wasmer_result_t::WASMER_OK
        }
        Err(_) => {
            update_last_error(CApiError {
                msg: "Failed to serialize the module".to_string(),
            });
            wasmer_result_t::WASMER_ERROR
        }
    }
}

/// Get bytes of the serialized module.
#[allow(clippy::cast_ptr_alignment)]
#[no_mangle]
pub unsafe extern "C" fn wasmer_serialized_module_bytes(
    serialized_module: *const wasmer_serialized_module_t,
) -> wasmer_byte_array {
    let serialized_module = &*(serialized_module as *const &[u8]);

    wasmer_byte_array {
        bytes: serialized_module.as_ptr(),
        bytes_len: serialized_module.len() as u32,
    }
}

/// Transform a sequence of bytes into a serialized module.
///
/// The caller owns the object and should call `wasmer_serialized_module_destroy` to free it.
///
/// Returns `wasmer_result_t::WASMER_OK` upon success.
///
/// Returns `wasmer_result_t::WASMER_ERROR` upon failure. Use `wasmer_last_error_length`
/// and `wasmer_last_error_message` to get an error message.
#[allow(clippy::cast_ptr_alignment)]
#[no_mangle]
pub unsafe extern "C" fn wasmer_serialized_module_from_bytes(
    serialized_module: *mut *mut wasmer_serialized_module_t,
    serialized_module_bytes: *const u8,
    serialized_module_bytes_length: u32,
) -> wasmer_result_t {
    if serialized_module.is_null() {
        update_last_error(CApiError {
            msg: "`serialized_module_bytes` pointer is null".to_string(),
        });
        return wasmer_result_t::WASMER_ERROR;
    }

    let serialized_module_bytes: &[u8] = slice::from_raw_parts(
        serialized_module_bytes,
        serialized_module_bytes_length as usize,
    );

    *serialized_module = Box::into_raw(Box::new(serialized_module_bytes)) as _;
    wasmer_result_t::WASMER_OK
}

/// Deserialize the given serialized module.
///
/// Returns `wasmer_result_t::WASMER_OK` upon success.
///
/// Returns `wasmer_result_t::WASMER_ERROR` upon failure. Use `wasmer_last_error_length`
/// and `wasmer_last_error_message` to get an error message.
#[allow(dead_code, unused_variables)]
#[allow(clippy::cast_ptr_alignment)]
#[no_mangle]
pub unsafe extern "C" fn wasmer_module_deserialize(
    module: *mut *mut wasmer_module_t,
    serialized_module: Option<&wasmer_serialized_module_t>,
) -> wasmer_result_t {
    let serialized_module: &[u8] = if let Some(sm) = serialized_module {
        &*(sm as *const wasmer_serialized_module_t as *const &[u8])
    } else {
        update_last_error(CApiError {
            msg: "`serialized_module` pointer is null".to_string(),
        });
        return wasmer_result_t::WASMER_ERROR;
    };

    let store = crate::get_global_store();

    match Module::deserialize(store, serialized_module) {
        Ok(deserialized_module) => {
            *module = Box::into_raw(Box::new(deserialized_module)) as _;
            wasmer_result_t::WASMER_OK
        }
        Err(e) => {
            update_last_error(CApiError { msg: e.to_string() });
            wasmer_result_t::WASMER_ERROR
        }
    }
}

/// Frees memory for the given serialized Module.
#[allow(clippy::cast_ptr_alignment)]
#[no_mangle]
pub extern "C" fn wasmer_serialized_module_destroy(
    serialized_module: *mut wasmer_serialized_module_t,
) {
    // TODO(mark): review all serialized logic memory logic
    if !serialized_module.is_null() {
        unsafe { Box::from_raw(serialized_module as *mut &[u8]) };
    }
}

/// Frees memory for the given Module
#[allow(clippy::cast_ptr_alignment)]
#[no_mangle]
pub extern "C" fn wasmer_module_destroy(module: *mut wasmer_module_t) {
    if !module.is_null() {
        unsafe { Box::from_raw(module as *mut Module) };
    }
}
