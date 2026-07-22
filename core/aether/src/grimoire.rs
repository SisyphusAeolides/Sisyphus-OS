// BOULDER SYSCALL GRIMOIRE
// Single source of truth. No module ever defines SYSCALL_* locally again.
// Numbering bands are locked by hardware interrupt vector proximity.
// Odd numbers are idempotent. Even numbers mutate state. Always.
// (This is a convention, not enforced by hardware — but the kernel respects it.)


pub const SYS_WRITE:          usize = 1;
pub const SYS_READ:           usize = 2;
pub const SYS_OPEN:           usize = 3;
pub const SYS_CLOSE:          usize = 4;
pub const SYS_SEEK:           usize = 5;
pub const SYS_IOCTL:          usize = 6;
pub const SYS_POLL:           usize = 7;
pub const SYS_DUP:            usize = 8;
pub const SYS_PIPE:           usize = 9;
pub const SYS_FSTAT:          usize = 10;
pub const SYS_TRUNCATE:       usize = 11;
pub const SYS_SYNC:           usize = 12;

pub const SYS_EXIT:           usize = 16;
pub const SYS_YIELD:          usize = 17;
pub const SYS_SPAWN:          usize = 18;
pub const SYS_WAIT:           usize = 19;
pub const SYS_GETPID:         usize = 20;
pub const SYS_GETPPID:        usize = 21;
pub const SYS_SIGNAL_DELIVER: usize = 22;
pub const SYS_SETPRIO:        usize = 23;
pub const SYS_GETPRIO:        usize = 24;
pub const SYS_THREAD_SPAWN:   usize = 25;
pub const SYS_THREAD_EXIT:    usize = 26;

pub const SYS_MMAP:           usize = 32;
pub const SYS_MUNMAP:         usize = 33;
pub const SYS_MPROTECT:       usize = 34;
pub const SYS_BRKSLAB:        usize = 35;
pub const SYS_SHMAP:          usize = 36;
pub const SYS_SHUNMAP:        usize = 37;
pub const SYS_MADVISE:        usize = 38;

pub const SYS_CLOCK_NOW:      usize = 48;
pub const SYS_CLOCK_SLEEP:    usize = 49;
pub const SYS_CLOCK_RES:      usize = 50;
pub const SYS_TACHYON_YIELD:  usize = 51;

pub const SYS_AOPEN:          usize = 64;
pub const SYS_AREAD:          usize = 65;
pub const SYS_AWRITE:         usize = 66;
pub const SYS_ACLOSE:         usize = 67;
pub const SYS_ASEEK:          usize = 68;
pub const SYS_AMKDIR:         usize = 69;
pub const SYS_AUNLINK:        usize = 70;
pub const SYS_ARENAME:        usize = 71;
pub const SYS_AREADDIR:       usize = 72;
pub const SYS_ASTAT:          usize = 73;

pub const SYS_CAP_SEND:       usize = 80;
pub const SYS_CAP_RECV:       usize = 81;
pub const SYS_CAP_GRANT:      usize = 82;
pub const SYS_CAP_REVOKE:     usize = 83;
pub const SYS_CHANNEL:        usize = 84;

pub const SYS_DISP_QUERY:     usize = 96;
pub const SYS_DISP_LEASE:     usize = 97;
pub const SYS_DISP_PRESENT:   usize = 98;
pub const SYS_DISP_BEAM:      usize = 99;

pub const SYS_NET_BIND:       usize = 112;
pub const SYS_NET_SEND:       usize = 113;
pub const SYS_NET_RECV:       usize = 114;
pub const SYS_NET_CLOSE:      usize = 115;
