//! Define `NativeArtifact` to allow compiling and instantiating to be
//! done as separate steps.

use crate::engine::{NativeEngine, NativeEngineInner};
use crate::serialize::ModuleMetadata;
use libloading::{Library, Symbol as LibrarySymbol};
use std::error::Error;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
#[cfg(feature = "compiler")]
use std::process::Command;
use std::sync::Arc;
use tempfile::NamedTempFile;
#[cfg(feature = "compiler")]
use tracing::trace;
use wasmer_compiler::{CompileError, Features, OperatingSystem, Symbol, SymbolRegistry, Triple};
#[cfg(feature = "compiler")]
use wasmer_compiler::{
    CompileModuleInfo, FunctionBodyData, ModuleEnvironment, ModuleTranslationState,
};
use wasmer_engine::{
    Artifact, DeserializeError, InstantiationError, LinkError, RuntimeError, SerializeError,
};
#[cfg(feature = "compiler")]
use wasmer_engine::{Engine, Tunables};
#[cfg(feature = "compiler")]
use wasmer_object::{emit_compilation, emit_data, get_object_for_target};
use wasmer_types::entity::{BoxedSlice, PrimaryMap};
#[cfg(feature = "compiler")]
use wasmer_types::DataInitializer;
use wasmer_types::{
    FunctionIndex, LocalFunctionIndex, MemoryIndex, OwnedDataInitializer, SignatureIndex,
    TableIndex,
};
use wasmer_vm::{
    FunctionBodyPtr, MemoryStyle, ModuleInfo, TableStyle, VMFunctionBody, VMSharedSignatureIndex,
    VMTrampoline,
};

/// A compiled wasm module, ready to be instantiated.
pub struct NativeArtifact {
    sharedobject_path: PathBuf,
    metadata: ModuleMetadata,
    #[allow(dead_code)]
    library: Option<Library>,
    finished_functions: BoxedSlice<LocalFunctionIndex, FunctionBodyPtr>,
    finished_dynamic_function_trampolines: BoxedSlice<FunctionIndex, FunctionBodyPtr>,
    signatures: BoxedSlice<SignatureIndex, VMSharedSignatureIndex>,
}

fn to_compile_error(err: impl Error) -> CompileError {
    CompileError::Codegen(format!("{}", err))
}

const WASMER_METADATA_SYMBOL: &[u8] = b"WASMER_METADATA";

impl NativeArtifact {
    // Mach-O header in Mac
    #[allow(dead_code)]
    const MAGIC_HEADER_MH_CIGAM_64: &'static [u8] = &[207, 250, 237, 254];

    // ELF Magic header for Linux (32 bit)
    #[allow(dead_code)]
    const MAGIC_HEADER_ELF_32: &'static [u8] = &[0x7f, b'E', b'L', b'F', 1];

    // ELF Magic header for Linux (64 bit)
    #[allow(dead_code)]
    const MAGIC_HEADER_ELF_64: &'static [u8] = &[0x7f, b'E', b'L', b'F', 2];

    // COFF Magic header for Windows (64 bit)
    #[allow(dead_code)]
    const MAGIC_HEADER_COFF_64: &'static [u8] = &[b'M', b'Z'];

    /// Check if the provided bytes look like `NativeArtifact`.
    ///
    /// This means, if the bytes look like a shared object file in the target
    /// system.
    pub fn is_deserializable(bytes: &[u8]) -> bool {
        cfg_if::cfg_if! {
            if #[cfg(all(target_pointer_width = "64", target_os="macos"))] {
                bytes.starts_with(Self::MAGIC_HEADER_MH_CIGAM_64)
            }
            else if #[cfg(all(target_pointer_width = "64", target_os="linux"))] {
                bytes.starts_with(Self::MAGIC_HEADER_ELF_64)
            }
            else if #[cfg(all(target_pointer_width = "32", target_os="linux"))] {
                bytes.starts_with(Self::MAGIC_HEADER_ELF_32)
            }
            else if #[cfg(all(target_pointer_width = "64", target_os="windows"))] {
                bytes.starts_with(Self::MAGIC_HEADER_COFF_64)
            }
            else {
                false
            }
        }
    }

    #[cfg(feature = "compiler")]
    /// Generate a compilation
    fn generate_metadata<'data>(
        data: &'data [u8],
        features: &Features,
        tunables: &dyn Tunables,
    ) -> Result<
        (
            CompileModuleInfo,
            PrimaryMap<LocalFunctionIndex, FunctionBodyData<'data>>,
            Vec<DataInitializer<'data>>,
            Option<ModuleTranslationState>,
        ),
        CompileError,
    > {
        let environ = ModuleEnvironment::new();
        let translation = environ.translate(data).map_err(CompileError::Wasm)?;
        let memory_styles: PrimaryMap<MemoryIndex, MemoryStyle> = translation
            .module
            .memories
            .values()
            .map(|memory_type| tunables.memory_style(memory_type))
            .collect();
        let table_styles: PrimaryMap<TableIndex, TableStyle> = translation
            .module
            .tables
            .values()
            .map(|table_type| tunables.table_style(table_type))
            .collect();

        let compile_info = CompileModuleInfo {
            module: Arc::new(translation.module),
            features: features.clone(),
            memory_styles,
            table_styles,
        };
        Ok((
            compile_info,
            translation.function_body_inputs,
            translation.data_initializers,
            translation.module_translation,
        ))
    }

    /// Compile a data buffer into a `NativeArtifact`, which may then be instantiated.
    #[cfg(feature = "compiler")]
    pub fn new(
        engine: &NativeEngine,
        data: &[u8],
        tunables: &dyn Tunables,
    ) -> Result<Self, CompileError> {
        let mut engine_inner = engine.inner_mut();
        let target = engine.target();
        let compiler = engine_inner.compiler()?;
        let (compile_info, function_body_inputs, data_initializers, module_translation) =
            Self::generate_metadata(data, engine_inner.features(), tunables)?;

        let data_initializers = data_initializers
            .iter()
            .map(OwnedDataInitializer::new)
            .collect::<Vec<_>>()
            .into_boxed_slice();

        let target_triple = target.triple();

        /*
        // We construct the function body lengths
        let function_body_lengths = compilation
            .get_function_bodies()
            .values()
            .map(|function_body| function_body.body.len() as u64)
            .map(|_function_body| 0u64)
            .collect::<PrimaryMap<LocalFunctionIndex, u64>>();
         */

        // TODO: we currently supply all-zero function body lengths.
        // We don't know the lengths until they're compiled, yet we have to
        // supply the metadata as an input to the compile.
        let function_body_lengths = function_body_inputs
            .keys()
            .map(|_function_body| 0u64)
            .collect::<PrimaryMap<LocalFunctionIndex, u64>>();

        let metadata = ModuleMetadata {
            compile_info,
            prefix: engine_inner.get_prefix(&data),
            data_initializers,
            function_body_lengths,
        };

        let serialized_data = bincode::serialize(&metadata).map_err(to_compile_error)?;
        let mut metadata_binary = vec![0; 10];
        let mut writable = &mut metadata_binary[..];
        leb128::write::unsigned(&mut writable, serialized_data.len() as u64)
            .expect("Should write number");
        metadata_binary.extend(serialized_data);

        let maybe_obj_bytes = compiler.experimental_native_compile_module(
            &target,
            &metadata.compile_info,
            module_translation.as_ref().unwrap(),
            &function_body_inputs,
            &metadata,
            &metadata_binary,
        );

        let filepath = match maybe_obj_bytes {
            Some(obj_bytes) => {
                let obj_bytes = obj_bytes?;
                let file = tempfile::Builder::new()
                    .prefix("wasmer_native")
                    .suffix(".o")
                    .tempfile()
                    .map_err(to_compile_error)?;

                // Re-open it.
                let (mut file, filepath) = file.keep().map_err(to_compile_error)?;
                file.write(&obj_bytes).map_err(to_compile_error)?;
                filepath
            }
            None => {
                let compilation = compiler.compile_module(
                    &target,
                    &metadata.compile_info,
                    module_translation.as_ref().unwrap(),
                    function_body_inputs,
                )?;
                let mut obj = get_object_for_target(&target_triple).map_err(to_compile_error)?;
                emit_data(&mut obj, WASMER_METADATA_SYMBOL, &metadata_binary)
                    .map_err(to_compile_error)?;
                emit_compilation(&mut obj, compilation, &metadata, &target_triple)
                    .map_err(to_compile_error)?;
                let file = tempfile::Builder::new()
                    .prefix("wasmer_native")
                    .suffix(".o")
                    .tempfile()
                    .map_err(to_compile_error)?;

                // Re-open it.
                let (mut file, filepath) = file.keep().map_err(to_compile_error)?;
                let obj_bytes = obj.write().map_err(to_compile_error)?;

                file.write(&obj_bytes).map_err(to_compile_error)?;
                filepath
            }
        };

        let shared_filepath = {
            let suffix = format!(".{}", Self::get_default_extension(&target_triple));
            let shared_file = tempfile::Builder::new()
                .prefix("wasmer_native")
                .suffix(&suffix)
                .tempfile()
                .map_err(to_compile_error)?;
            shared_file
                .into_temp_path()
                .keep()
                .map_err(to_compile_error)?
        };

        let host_target = Triple::host();
        let is_cross_compiling = target_triple != &host_target;
        let cross_compiling_args: Vec<String> = if is_cross_compiling {
            vec![
                format!("--target={}", target_triple),
                "-fuse-ld=lld".to_string(),
                "-nodefaultlibs".to_string(),
                "-nostdlib".to_string(),
            ]
        } else {
            vec![]
        };
        let target_args = match (target_triple.operating_system, is_cross_compiling) {
            (OperatingSystem::Windows, true) => vec!["-Wl,/force:unresolved,/noentry"],
            (OperatingSystem::Windows, false) => vec!["-Wl,-undefined,dynamic_lookup"],
            _ => vec!["-nostartfiles", "-Wl,-undefined,dynamic_lookup"],
        };
        trace!(
            "Compiling for target {} from host {}",
            target_triple.to_string(),
            host_target.to_string()
        );

        let linker = if is_cross_compiling {
            "clang-10"
        } else {
            "gcc"
        };

        let output = Command::new(linker)
            .arg(&filepath)
            .arg("-o")
            .arg(&shared_filepath)
            .args(&target_args)
            // .args(&wasmer_symbols)
            .arg("-shared")
            .args(&cross_compiling_args)
            .arg("-v")
            .output()
            .map_err(to_compile_error)?;

        if !output.status.success() {
            return Err(CompileError::Codegen(format!(
                "Shared object file generator failed with:\nstderr:{}\nstdout:{}",
                String::from_utf8_lossy(&output.stderr).trim_end(),
                String::from_utf8_lossy(&output.stdout).trim_end()
            )));
        }
        trace!("gcc command result {:?}", output);
        if is_cross_compiling {
            Self::from_parts_crosscompiled(metadata, shared_filepath)
        } else {
            let lib = Library::new(&shared_filepath).map_err(to_compile_error)?;
            Self::from_parts(&mut engine_inner, metadata, shared_filepath, lib)
        }
    }

    /// Get the default extension when serializing this artifact
    pub fn get_default_extension(triple: &Triple) -> &'static str {
        match triple.operating_system {
            OperatingSystem::Windows => "dll",
            OperatingSystem::Darwin | OperatingSystem::Ios | OperatingSystem::MacOSX { .. } => {
                "dylib"
            }
            _ => "so",
        }
    }

    /// Construct a `NativeArtifact` from component parts.
    pub fn from_parts_crosscompiled(
        metadata: ModuleMetadata,
        sharedobject_path: PathBuf,
    ) -> Result<Self, CompileError> {
        let finished_functions: PrimaryMap<LocalFunctionIndex, FunctionBodyPtr> = PrimaryMap::new();
        let finished_dynamic_function_trampolines: PrimaryMap<FunctionIndex, FunctionBodyPtr> =
            PrimaryMap::new();
        let signatures: PrimaryMap<SignatureIndex, VMSharedSignatureIndex> = PrimaryMap::new();
        Ok(Self {
            sharedobject_path,
            metadata,
            library: None,
            finished_functions: finished_functions.into_boxed_slice(),
            finished_dynamic_function_trampolines: finished_dynamic_function_trampolines
                .into_boxed_slice(),
            signatures: signatures.into_boxed_slice(),
        })
    }

    /// Construct a `NativeArtifact` from component parts.
    pub fn from_parts(
        engine_inner: &mut NativeEngineInner,
        metadata: ModuleMetadata,
        sharedobject_path: PathBuf,
        lib: Library,
    ) -> Result<Self, CompileError> {
        let mut finished_functions: PrimaryMap<LocalFunctionIndex, FunctionBodyPtr> =
            PrimaryMap::new();
        for (function_local_index, function_len) in metadata.function_body_lengths.iter() {
            let function_name =
                metadata.symbol_to_name(Symbol::LocalFunction(function_local_index));
            unsafe {
                // We use a fake function signature `fn()` because we just
                // want to get the function address.
                let func: LibrarySymbol<unsafe extern "C" fn()> = lib
                    .get(function_name.as_bytes())
                    .map_err(to_compile_error)?;
                let raw = *func.into_raw();
                // The function pointer is a fat pointer, however this information
                // is only used when retrieving the trap information which is not yet
                // implemented in this engine.
                let func_pointer =
                    std::slice::from_raw_parts(raw as *const (), *function_len as usize);
                let func_pointer = func_pointer as *const [()] as *mut [VMFunctionBody];
                finished_functions.push(FunctionBodyPtr(func_pointer));
            }
        }

        // Retrieve function call trampolines (for all signatures in the module)
        for (sig_index, func_type) in metadata.compile_info.module.signatures.iter() {
            let function_name = metadata.symbol_to_name(Symbol::FunctionCallTrampoline(sig_index));
            unsafe {
                let trampoline: LibrarySymbol<VMTrampoline> = lib
                    .get(function_name.as_bytes())
                    .map_err(to_compile_error)?;
                engine_inner.add_trampoline(&func_type, *trampoline);
            }
        }

        // Retrieve dynamic function trampolines (only for imported functions)
        let mut finished_dynamic_function_trampolines: PrimaryMap<FunctionIndex, FunctionBodyPtr> =
            PrimaryMap::with_capacity(metadata.compile_info.module.num_imported_functions);
        for func_index in metadata
            .compile_info
            .module
            .functions
            .keys()
            .take(metadata.compile_info.module.num_imported_functions)
        {
            let function_name =
                metadata.symbol_to_name(Symbol::DynamicFunctionTrampoline(func_index));
            unsafe {
                let trampoline: LibrarySymbol<unsafe extern "C" fn()> = lib
                    .get(function_name.as_bytes())
                    .map_err(to_compile_error)?;
                let raw = *trampoline.into_raw();
                let trampoline_pointer = std::slice::from_raw_parts(raw as *const (), 0);
                let trampoline_pointer =
                    trampoline_pointer as *const [()] as *mut [VMFunctionBody];
                finished_dynamic_function_trampolines.push(FunctionBodyPtr(trampoline_pointer));
            }
        }

        // Leaving frame infos from now, as they are not yet used
        // however they might be useful for the future.
        // let frame_infos = compilation
        //     .get_frame_info()
        //     .values()
        //     .map(|frame_info| SerializableFunctionFrameInfo::Processed(frame_info.clone()))
        //     .collect::<PrimaryMap<LocalFunctionIndex, _>>();
        // Self::from_parts(&mut engine_inner, lib, metadata, )
        // let frame_info_registration = register_frame_info(
        //     serializable.module.clone(),
        //     &finished_functions,
        //     serializable.compilation.function_frame_info.clone(),
        // );

        // Compute indices into the shared signature table.
        let signatures = {
            let signature_registry = engine_inner.signatures();
            metadata
                .compile_info
                .module
                .signatures
                .values()
                .map(|sig| signature_registry.register(sig))
                .collect::<PrimaryMap<_, _>>()
        };

        Ok(Self {
            sharedobject_path,
            metadata,
            library: Some(lib),
            finished_functions: finished_functions.into_boxed_slice(),
            finished_dynamic_function_trampolines: finished_dynamic_function_trampolines
                .into_boxed_slice(),
            signatures: signatures.into_boxed_slice(),
        })
    }

    /// Compile a data buffer into a `NativeArtifact`, which may then be instantiated.
    #[cfg(not(feature = "compiler"))]
    pub fn new(_engine: &NativeEngine, _data: &[u8]) -> Result<Self, CompileError> {
        Err(CompileError::Codegen(
            "Compilation is not enabled in the engine".to_string(),
        ))
    }

    /// Deserialize a `NativeArtifact` from bytes.
    ///
    /// # Safety
    ///
    /// The bytes must represent a serialized WebAssembly module.
    pub unsafe fn deserialize(
        engine: &NativeEngine,
        bytes: &[u8],
    ) -> Result<Self, DeserializeError> {
        if !Self::is_deserializable(&bytes) {
            return Err(DeserializeError::Incompatible(
                "The provided bytes are not in any native format Wasmer can understand".to_string(),
            ));
        }
        // Dump the bytes into a file, so we can read it with our `dlopen`
        let named_file = NamedTempFile::new()?;
        let (mut file, path) = named_file.keep().map_err(|e| e.error)?;
        file.write_all(&bytes)?;
        // We already checked for the header, so we don't need
        // to check again.
        Self::deserialize_from_file_unchecked(&engine, &path)
    }

    /// Deserialize a `NativeArtifact` from a file path.
    ///
    /// # Safety
    ///
    /// The file's content must represent a serialized WebAssembly module.
    pub unsafe fn deserialize_from_file(
        engine: &NativeEngine,
        path: &Path,
    ) -> Result<Self, DeserializeError> {
        let mut file = File::open(&path)?;
        let mut buffer = [0; 5];
        // read up to 5 bytes
        file.read_exact(&mut buffer)?;
        if !Self::is_deserializable(&buffer) {
            return Err(DeserializeError::Incompatible(
                "The provided bytes are not in any native format Wasmer can understand".to_string(),
            ));
        }
        Self::deserialize_from_file_unchecked(&engine, &path)
    }

    /// Deserialize a `NativeArtifact` from a file path (unchecked).
    ///
    /// # Safety
    ///
    /// The file's content must represent a serialized WebAssembly module.
    pub unsafe fn deserialize_from_file_unchecked(
        engine: &NativeEngine,
        path: &Path,
    ) -> Result<Self, DeserializeError> {
        let lib = Library::new(&path).map_err(|e| {
            DeserializeError::CorruptedBinary(format!("Library loading failed: {}", e))
        })?;
        let shared_path: PathBuf = PathBuf::from(path);
        // We use 10 + 1, as the length of the module will take 10 bytes
        // (we construct it like that in `metadata_length`) and we also want
        // to take the first element of the data to construct the slice from
        // it.
        let symbol: LibrarySymbol<*mut [u8; 10 + 1]> =
            lib.get(WASMER_METADATA_SYMBOL).map_err(|e| {
                DeserializeError::CorruptedBinary(format!(
                    "The provided object file doesn't seem to be generated by Wasmer: {}",
                    e
                ))
            })?;
        use std::ops::Deref;
        use std::slice;

        let size = &mut **symbol.deref();
        let mut readable = &size[..];
        let metadata_len = leb128::read::unsigned(&mut readable).map_err(|_e| {
            DeserializeError::CorruptedBinary("Can't read metadata size".to_string())
        })?;
        let metadata_slice: &'static [u8] =
            slice::from_raw_parts(&size[10] as *const u8, metadata_len as usize);
        let metadata: ModuleMetadata = bincode::deserialize(metadata_slice)
            .map_err(|e| DeserializeError::CorruptedBinary(format!("{:?}", e)))?;
        let mut engine_inner = engine.inner_mut();

        Self::from_parts(&mut engine_inner, metadata, shared_path, lib)
            .map_err(DeserializeError::Compiler)
    }
}

impl Artifact for NativeArtifact {
    fn module(&self) -> Arc<ModuleInfo> {
        self.metadata.compile_info.module.clone()
    }

    fn module_ref(&self) -> &ModuleInfo {
        &self.metadata.compile_info.module
    }

    fn module_mut(&mut self) -> Option<&mut ModuleInfo> {
        Arc::get_mut(&mut self.metadata.compile_info.module)
    }

    fn register_frame_info(&self) {
        // Do nothing for now
    }

    fn features(&self) -> &Features {
        &self.metadata.compile_info.features
    }

    fn data_initializers(&self) -> &[OwnedDataInitializer] {
        &*self.metadata.data_initializers
    }

    fn memory_styles(&self) -> &PrimaryMap<MemoryIndex, MemoryStyle> {
        &self.metadata.compile_info.memory_styles
    }

    fn table_styles(&self) -> &PrimaryMap<TableIndex, TableStyle> {
        &self.metadata.compile_info.table_styles
    }

    fn finished_functions(&self) -> &BoxedSlice<LocalFunctionIndex, FunctionBodyPtr> {
        &self.finished_functions
    }

    fn finished_dynamic_function_trampolines(&self) -> &BoxedSlice<FunctionIndex, FunctionBodyPtr> {
        &self.finished_dynamic_function_trampolines
    }

    fn signatures(&self) -> &BoxedSlice<SignatureIndex, VMSharedSignatureIndex> {
        &self.signatures
    }

    fn preinstantiate(&self) -> Result<(), InstantiationError> {
        if self.library.is_none() {
            return Err(InstantiationError::Link(LinkError::Trap(
                RuntimeError::new("Cross compiled artifacts can't be instantiated."),
            )));
        }
        Ok(())
    }

    /// Serialize a NativeArtifact
    fn serialize(&self) -> Result<Vec<u8>, SerializeError> {
        Ok(std::fs::read(&self.sharedobject_path)?)
    }
}
