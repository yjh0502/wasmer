use super::frame_info::{FrameInfo, GlobalFrameInfo, FRAME_INFO};
use backtrace::Backtrace;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::RwLockReadGuard;
use wasmer_vm::{raise_user_trap, Trap, TrapCode};

/// A struct representing an aborted instruction execution, with a message
/// indicating the cause.
#[derive(Clone)]
pub struct RuntimeError {
    inner: Arc<RuntimeErrorInner>,
}

/// The source of the `RuntimeError`.
#[derive(Debug)]
enum RuntimeErrorSource {
    Generic(String),
    User(Box<dyn Error + Send + Sync>),
    Trap(TrapCode),
}

impl fmt::Display for RuntimeErrorSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Generic(s) => write!(f, "{}", s),
            Self::User(s) => write!(f, "{}", s),
            Self::Trap(s) => write!(f, "{}", s.message()),
        }
    }
}

struct RuntimeErrorInner {
    /// The source error (this can be a custom user `Error` or a [`TrapCode`])
    source: RuntimeErrorSource,
    /// The reconstructed Wasm trace (from the native trace and the `GlobalFrameInfo`).
    wasm_trace: Vec<FrameInfo>,
    /// The native backtrace
    native_trace: Backtrace,
}

fn _assert_trap_is_sync_and_send(t: &Trap) -> (&dyn Sync, &dyn Send) {
    (t, t)
}

impl RuntimeError {
    /// Creates a new generic `RuntimeError` with the given `message`.
    ///
    /// # Example
    /// ```
    /// let trap = wasmer_engine::RuntimeError::new("unexpected error");
    /// assert_eq!("unexpected error", trap.message());
    /// ```
    pub fn new<I: Into<String>>(message: I) -> Self {
        let info = FRAME_INFO.read().unwrap();
        let msg = message.into();
        Self::new_with_trace(
            info,
            None,
            RuntimeErrorSource::Generic(msg),
            Backtrace::new_unresolved(),
        )
    }

    /// Create a new RuntimeError from a Trap.
    pub fn from_trap(trap: Trap) -> Self {
        let info = FRAME_INFO.read().unwrap();
        match trap {
            Trap::User(error) => {
                match error.downcast::<RuntimeError>() {
                    // The error is already a RuntimeError, we return it directly
                    Ok(runtime_error) => *runtime_error,
                    Err(e) => Self::new_with_trace(
                        info,
                        None,
                        RuntimeErrorSource::User(e),
                        Backtrace::new_unresolved(),
                    ),
                }
            }
            // A trap caused by an error on the generated machine code for a Wasm function
            Trap::Wasm {
                pc,
                signal_trap,
                backtrace,
            } => {
                let info = if info.should_process_frame(pc).unwrap_or(false) {
                    drop(info);
                    let mut info = FRAME_INFO.write().unwrap();
                    info.maybe_process_frame(pc).unwrap();
                    drop(info);
                    FRAME_INFO.read().unwrap()
                } else {
                    info
                };
                let code = info
                    .lookup_trap_info(pc)
                    .map_or(signal_trap.unwrap_or(TrapCode::StackOverflow), |info| {
                        info.trap_code
                    });
                Self::new_with_trace(info, Some(pc), RuntimeErrorSource::Trap(code), backtrace)
            }
            // A trap triggered manually from the Wasmer runtime
            Trap::Runtime {
                trap_code,
                backtrace,
            } => Self::new_with_trace(info, None, RuntimeErrorSource::Trap(trap_code), backtrace),
        }
    }

    /// Raises a custom user Error
    pub fn raise(error: Box<dyn Error + Send + Sync>) -> ! {
        unsafe { raise_user_trap(error) }
    }

    fn new_with_trace(
        info: RwLockReadGuard<GlobalFrameInfo>,
        trap_pc: Option<usize>,
        source: RuntimeErrorSource,
        native_trace: Backtrace,
    ) -> Self {
        let frames: Vec<usize> = native_trace
            .frames()
            .iter()
            .filter_map(|frame| {
                let pc = frame.ip() as usize;
                if pc == 0 {
                    None
                } else {
                    // Note that we need to be careful about the pc we pass in here to
                    // lookup frame information. This program counter is used to
                    // translate back to an original source location in the origin wasm
                    // module. If this pc is the exact pc that the trap happened at,
                    // then we look up that pc precisely. Otherwise backtrace
                    // information typically points at the pc *after* the call
                    // instruction (because otherwise it's likely a call instruction on
                    // the stack). In that case we want to lookup information for the
                    // previous instruction (the call instruction) so we subtract one as
                    // the lookup.
                    let pc_to_lookup = if Some(pc) == trap_pc { pc } else { pc - 1 };
                    Some(pc_to_lookup)
                }
            })
            .collect();

        // If any of the frames is not processed, we adquire the lock to
        // modify the GlobalFrameInfo module.
        let info = if frames
            .iter()
            .any(|pc| info.should_process_frame(*pc).unwrap_or(false))
        {
            // We drop the read lock, to get a write one.
            // Note: this is not guaranteed because it's a RwLock:
            // the following code may cause deadlocks.
            // TODO: clean up this code
            drop(info);
            {
                let mut info = FRAME_INFO.write().unwrap();
                for pc in frames.iter() {
                    info.maybe_process_frame(*pc);
                }
            }
            FRAME_INFO.read().unwrap()
        } else {
            info
        };

        // Let's construct the trace
        let wasm_trace = frames
            .into_iter()
            .filter_map(|pc| info.lookup_frame_info(pc))
            .collect::<Vec<_>>();

        Self {
            inner: Arc::new(RuntimeErrorInner {
                source,
                wasm_trace,
                native_trace,
            }),
        }
    }

    /// Returns a reference the `message` stored in `Trap`.
    pub fn message(&self) -> String {
        format!("{}", self.inner.source)
    }

    /// Returns a list of function frames in WebAssembly code that led to this
    /// trap happening.
    pub fn trace(&self) -> &[FrameInfo] {
        &self.inner.wasm_trace
    }

    /// Attempts to downcast the `RuntimeError` to a concrete type.
    pub fn downcast<T: Error + 'static>(self) -> Result<T, Self> {
        match Arc::try_unwrap(self.inner) {
            // We only try to downcast user errors
            Ok(RuntimeErrorInner {
                source: RuntimeErrorSource::User(err),
                ..
            }) if err.is::<T>() => Ok(*err.downcast::<T>().unwrap()),
            Ok(inner) => Err(Self {
                inner: Arc::new(inner),
            }),
            Err(inner) => Err(Self { inner }),
        }
    }

    /// Returns true if the `RuntimeError` is the same as T
    pub fn is<T: Error + 'static>(&self) -> bool {
        match &self.inner.source {
            RuntimeErrorSource::User(err) => err.is::<T>(),
            _ => false,
        }
    }
}

impl fmt::Debug for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeError")
            .field("source", &self.inner.source)
            .field("wasm_trace", &self.inner.wasm_trace)
            .field("native_trace", &self.inner.native_trace)
            .finish()
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RuntimeError: {}", self.message())?;
        let trace = self.trace();
        if trace.is_empty() {
            return Ok(());
        }
        for frame in self.trace().iter() {
            let name = frame.module_name();
            let func_index = frame.func_index();
            writeln!(f)?;
            write!(f, "    at ")?;
            match frame.function_name() {
                Some(name) => match rustc_demangle::try_demangle(name) {
                    Ok(name) => write!(f, "{}", name)?,
                    Err(_) => write!(f, "{}", name)?,
                },
                None => write!(f, "<unnamed>")?,
            }
            write!(
                f,
                " ({}[{}]:0x{:x})",
                name,
                func_index,
                frame.module_offset()
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for RuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.inner.source {
            RuntimeErrorSource::User(err) => Some(&**err),
            RuntimeErrorSource::Trap(err) => Some(err),
            _ => None,
        }
    }
}

impl From<Trap> for RuntimeError {
    fn from(trap: Trap) -> Self {
        Self::from_trap(trap)
    }
}
