use core::marker::PhantomData;
use core::ptr::NonNull;

use sisyphus_driver_abi::{STATUS_INVALID_ARGUMENT, Status};

use super::personality::CallingConvention;

const ABSOLUTE_TAIL_JUMP_LENGTH: usize = 12;

pub trait ThunkMemoryBackend: Sync {
    type Handle: Copy;

    fn allocate_writable(
        &self,
        minimum_size: usize,
    ) -> Result<WritableMapping<Self::Handle>, Status>;
    fn seal_executable(&self, handle: Self::Handle, used_size: usize) -> Result<u64, Status>;
    fn release(&self, handle: Self::Handle) -> Status;
}

#[derive(Clone, Copy)]
pub struct WritableMapping<Handle: Copy> {
    pub handle: Handle,
    pub pointer: NonNull<u8>,
    pub capacity: usize,
}

pub struct Writable;
pub struct Executable;

pub struct ThunkPage<'a, State, Backend: ThunkMemoryBackend + ?Sized> {
    backend: &'a Backend,
    handle: Backend::Handle,
    pointer: Option<NonNull<u8>>,
    capacity: usize,
    used: usize,
    entry_address: u64,
    active: bool,
    _state: PhantomData<State>,
}

impl<'a, Backend: ThunkMemoryBackend + ?Sized> ThunkPage<'a, Writable, Backend> {
    pub fn allocate(backend: &'a Backend, minimum_size: usize) -> Result<Self, Status> {
        if minimum_size == 0 {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        let mapping = backend.allocate_writable(minimum_size)?;
        if mapping.capacity < minimum_size {
            let _ = backend.release(mapping.handle);
            return Err(STATUS_INVALID_ARGUMENT);
        }
        Ok(Self {
            backend,
            handle: mapping.handle,
            pointer: Some(mapping.pointer),
            capacity: mapping.capacity,
            used: 0,
            entry_address: 0,
            active: true,
            _state: PhantomData,
        })
    }

    pub fn emit_same_abi_tail_thunk(
        &mut self,
        source: CallingConvention,
        target: CallingConvention,
        target_address: u64,
    ) -> Result<(), JitLinkError> {
        if source != target {
            return Err(JitLinkError::UnsupportedCallingConvention);
        }
        if target_address == 0 || self.capacity < ABSOLUTE_TAIL_JUMP_LENGTH {
            return Err(JitLinkError::InvalidTarget);
        }
        let mut code = [0_u8; ABSOLUTE_TAIL_JUMP_LENGTH];
        code[0] = 0x48;
        code[1] = 0xb8;
        code[2..10].copy_from_slice(&target_address.to_le_bytes());
        code[10] = 0xff;
        code[11] = 0xe0;
        // SAFETY: allocate_writable guaranteed a live writable mapping with at
        // least capacity bytes until seal_executable or release.
        unsafe {
            self.pointer
                .expect("writable thunk pointer")
                .as_ptr()
                .copy_from_nonoverlapping(code.as_ptr(), code.len());
        }
        self.used = code.len();
        Ok(())
    }

    pub fn finalize(mut self) -> Result<ThunkPage<'a, Executable, Backend>, Status> {
        if self.used == 0 {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        let entry_address = self.backend.seal_executable(self.handle, self.used)?;
        if entry_address == 0 {
            return Err(STATUS_INVALID_ARGUMENT);
        }
        self.pointer = None;
        self.active = false;
        Ok(ThunkPage {
            backend: self.backend,
            handle: self.handle,
            pointer: None,
            capacity: self.capacity,
            used: self.used,
            entry_address,
            active: true,
            _state: PhantomData,
        })
    }
}

impl<Backend: ThunkMemoryBackend + ?Sized> ThunkPage<'_, Executable, Backend> {
    pub const fn entry_address(&self) -> u64 {
        self.entry_address
    }

    pub fn release(mut self) -> Status {
        let status = self.backend.release(self.handle);
        if status == sisyphus_driver_abi::STATUS_OK {
            self.active = false;
        }
        status
    }
}

impl<State, Backend: ThunkMemoryBackend + ?Sized> Drop for ThunkPage<'_, State, Backend> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.backend.release(self.handle);
            self.active = false;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JitLinkError {
    InvalidTarget,
    UnsupportedCallingConvention,
}

#[cfg(test)]
mod tests {
    use core::cell::UnsafeCell;

    use sisyphus_driver_abi::STATUS_OK;

    use super::*;

    struct TestBackend {
        bytes: UnsafeCell<[u8; 64]>,
    }

    // SAFETY: The test uses one page serially and does not retain aliases.
    unsafe impl Sync for TestBackend {}

    impl ThunkMemoryBackend for TestBackend {
        type Handle = u64;

        fn allocate_writable(
            &self,
            _minimum_size: usize,
        ) -> Result<WritableMapping<Self::Handle>, Status> {
            let pointer = NonNull::new(self.bytes.get().cast::<u8>()).unwrap();
            Ok(WritableMapping {
                handle: 1,
                pointer,
                capacity: 64,
            })
        }

        fn seal_executable(&self, _handle: Self::Handle, _used_size: usize) -> Result<u64, Status> {
            Ok(0x8000)
        }

        fn release(&self, _handle: Self::Handle) -> Status {
            STATUS_OK
        }
    }

    #[test]
    fn emits_and_seals_a_same_abi_tail_jump() {
        let backend = TestBackend {
            bytes: UnsafeCell::new([0; 64]),
        };
        let mut page = ThunkPage::allocate(&backend, 16).unwrap();
        page.emit_same_abi_tail_thunk(
            CallingConvention::SystemV64,
            CallingConvention::SystemV64,
            0x1234_5678,
        )
        .unwrap();
        let executable = page.finalize().unwrap();
        assert_eq!(executable.entry_address(), 0x8000);
        let bytes = unsafe { &*backend.bytes.get() };
        assert_eq!(&bytes[..2], &[0x48, 0xb8]);
        assert_eq!(&bytes[10..12], &[0xff, 0xe0]);
        assert_eq!(executable.release(), STATUS_OK);
    }

    #[test]
    fn rejects_incomplete_cross_abi_translation() {
        let backend = TestBackend {
            bytes: UnsafeCell::new([0; 64]),
        };
        let mut page = ThunkPage::allocate(&backend, 16).unwrap();
        assert_eq!(
            page.emit_same_abi_tail_thunk(
                CallingConvention::Windows64,
                CallingConvention::SystemV64,
                0x1000,
            ),
            Err(JitLinkError::UnsupportedCallingConvention)
        );
    }
}
