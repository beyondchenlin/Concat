//! Receive JNI scalar booleans using their actual unsigned-byte return ABI.
//!
//! `jni-sys` 0.4.1 declares these function pointers as returning Rust `bool`.
//! Android AArch64 may leave bits above the returned byte unspecified, whereas
//! the Rust boolean ABI lets the optimizer assume a canonical register value.
//! Reinterpreting the function pointer *before* the call, then testing the byte,
//! avoids ever receiving the foreign result as a Rust boolean.
//!
//! This module has no crate dependencies. Its regression tests can also be run
//! with `rustc --edition=2024 --test -O src/ffi_bool.rs -o ffi-bool-tests`.
//! The dirty-register fixture is enabled only on Android AArch64.

macro_rules! define_byte_return_call {
    ($name:ident, $( $argument:ident: $type:ident ),+) => {
        /// Call a byte-returning JNI function stored in a legacy boolean slot.
        ///
        /// # Safety
        ///
        /// The function must actually use the system ABI with the given argument
        /// types and an unsigned-byte return value. The caller must satisfy the
        /// JNI function's usual environment, lifetime, argument and exception
        /// requirements. This is not a general adapter for Rust boolean functions.
        #[inline(always)]
        pub(crate) unsafe fn $name<$($type),+>(
            function: unsafe extern "system" fn($($type),+) -> bool,
            $($argument: $type),+
        ) -> bool {
            // The table is unchanged. Only the call site's return convention is
            // corrected; every argument type and the system ABI are preserved.
            let function: unsafe extern "system" fn($($type),+) -> u8 =
                unsafe { core::mem::transmute(function) };
            let byte = unsafe { function($($argument),+) };
            byte != 0
        }
    };
}

// Counts include JNIEnv. These are the four arities of all nine scalar boolean
// functions used by jni 0.22.4, not the unused raw variadic/V/virtual-thread API.
define_byte_return_call!(call1, a: A);
define_byte_return_call!(call3, a: A, b: B, c: C);
define_byte_return_call!(call4, a: A, b: B, c: C, d: D);
define_byte_return_call!(call5, a: A, b: B, c: C, d: D, e: E);

#[cfg(test)]
mod tests {
    use super::*;
    use core::hint::black_box;
    use core::sync::atomic::{AtomicUsize, Ordering};

    // These stubs really return bytes. The intermediate pointer type models
    // the existing jni-sys table; it is never invoked with a boolean return ABI.
    #[inline(never)]
    unsafe extern "system" fn byte_result(byte: u8) -> u8 {
        black_box(byte)
    }

    #[test]
    fn zero_is_false_and_all_nonzero_bytes_are_true() {
        let byte_function = black_box(byte_result as unsafe extern "system" fn(u8) -> u8);
        let legacy_slot: unsafe extern "system" fn(u8) -> bool =
            unsafe { core::mem::transmute(byte_function) };
        assert!(!unsafe { call1(legacy_slot, black_box(0)) });
        assert!(unsafe { call1(legacy_slot, black_box(1)) });
        for byte in 2..=u8::MAX {
            assert!(
                unsafe { call1(legacy_slot, black_box(byte)) },
                "byte {byte}"
            );
        }
    }

    static ARGUMENTS: [AtomicUsize; 5] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];

    unsafe extern "system" fn record1(a: usize) -> u8 {
        ARGUMENTS[0].store(a, Ordering::Relaxed);
        0x80
    }

    unsafe extern "system" fn record3(a: usize, b: *const u8, c: i64) -> u8 {
        ARGUMENTS[0].store(a, Ordering::Relaxed);
        ARGUMENTS[1].store(b as usize, Ordering::Relaxed);
        ARGUMENTS[2].store(c as usize, Ordering::Relaxed);
        0x80
    }

    unsafe extern "system" fn record4(a: usize, b: *const u8, c: i64, d: f64) -> u8 {
        ARGUMENTS[0].store(a, Ordering::Relaxed);
        ARGUMENTS[1].store(b as usize, Ordering::Relaxed);
        ARGUMENTS[2].store(c as usize, Ordering::Relaxed);
        ARGUMENTS[3].store(d.to_bits() as usize, Ordering::Relaxed);
        0x80
    }

    unsafe extern "system" fn record5(a: usize, b: *const u8, c: i64, d: f64, e: *mut u16) -> u8 {
        ARGUMENTS[0].store(a, Ordering::Relaxed);
        ARGUMENTS[1].store(b as usize, Ordering::Relaxed);
        ARGUMENTS[2].store(c as usize, Ordering::Relaxed);
        ARGUMENTS[3].store(d.to_bits() as usize, Ordering::Relaxed);
        ARGUMENTS[4].store(e as usize, Ordering::Relaxed);
        0x80
    }

    fn recorded(count: usize) -> Vec<usize> {
        ARGUMENTS[..count]
            .iter()
            .map(|argument| argument.load(Ordering::Relaxed))
            .collect()
    }

    #[test]
    fn every_used_arity_preserves_argument_types_order_and_values() {
        let a = black_box(0x1234_5678usize);
        let byte = 19u8;
        let b = &byte as *const u8;
        let c = black_box(-0x1122_3344_5566i64);
        let d = black_box(1234.5f64);
        let mut word = 37u16;
        let e = &mut word as *mut u16;
        let expected = [a, b as usize, c as usize, d.to_bits() as usize, e as usize];

        let slot1: unsafe extern "system" fn(usize) -> bool = unsafe {
            core::mem::transmute(black_box(record1 as unsafe extern "system" fn(usize) -> u8))
        };
        assert!(unsafe { call1(slot1, a) });
        assert_eq!(recorded(1), expected[..1]);

        let slot3: unsafe extern "system" fn(usize, *const u8, i64) -> bool = unsafe {
            core::mem::transmute(black_box(
                record3 as unsafe extern "system" fn(usize, *const u8, i64) -> u8,
            ))
        };
        assert!(unsafe { call3(slot3, a, b, c) });
        assert_eq!(recorded(3), expected[..3]);

        let slot4: unsafe extern "system" fn(usize, *const u8, i64, f64) -> bool = unsafe {
            core::mem::transmute(black_box(
                record4 as unsafe extern "system" fn(usize, *const u8, i64, f64) -> u8,
            ))
        };
        assert!(unsafe { call4(slot4, a, b, c, d) });
        assert_eq!(recorded(4), expected[..4]);

        let slot5: unsafe extern "system" fn(usize, *const u8, i64, f64, *mut u16) -> bool = unsafe {
            core::mem::transmute(black_box(
                record5 as unsafe extern "system" fn(usize, *const u8, i64, f64, *mut u16) -> u8,
            ))
        };
        assert!(unsafe { call5(slot5, a, b, c, d, e) });
        assert_eq!(recorded(5), expected);
    }

    // Two assembly entry symbols share the same body. One exposes the complete
    // word for fixture verification; the byte entry obeys JNI's actual return
    // contract while intentionally retaining the observed nonzero upper bits.
    // No Rust function with an invalid boolean result is ever called.
    #[cfg(all(target_os = "android", target_arch = "aarch64"))]
    core::arch::global_asm!(
        ".text",
        ".p2align 2",
        ".global jni_bool_test_dirty_byte",
        ".global jni_bool_test_dirty_word",
        ".type jni_bool_test_dirty_byte, %function",
        ".type jni_bool_test_dirty_word, %function",
        "jni_bool_test_dirty_byte:",
        "jni_bool_test_dirty_word:",
        "and x0, x0, #0xff",
        "mov x9, #0x3500",
        "movk x9, #0x7866, lsl #16",
        "movk x9, #0x7b89, lsl #32",
        "orr x0, x0, x9",
        "ret",
        ".size jni_bool_test_dirty_byte, .-jni_bool_test_dirty_byte",
        ".size jni_bool_test_dirty_word, .-jni_bool_test_dirty_word",
    );

    #[cfg(all(target_os = "android", target_arch = "aarch64"))]
    unsafe extern "system" {
        fn jni_bool_test_dirty_byte(byte: usize) -> u8;
        fn jni_bool_test_dirty_word(byte: usize) -> usize;
    }

    #[cfg(all(target_os = "android", target_arch = "aarch64"))]
    #[test]
    fn dirty_aarch64_return_register_uses_only_the_low_byte() {
        let byte_function =
            black_box(jni_bool_test_dirty_byte as unsafe extern "system" fn(usize) -> u8);
        let legacy_slot: unsafe extern "system" fn(usize) -> bool =
            unsafe { core::mem::transmute(byte_function) };

        let word = unsafe { jni_bool_test_dirty_word(0) };
        assert_eq!(word, 0x7b89_7866_3500);
        assert!(!unsafe { call1(legacy_slot, black_box(0)) });
        for byte in [1, 2, 0x80, 0xff] {
            let word = unsafe { jni_bool_test_dirty_word(byte) };
            assert_eq!(word & 0xff, byte);
            assert_eq!(word >> 8, 0x7b89_7866_35);
            assert!(unsafe { call1(legacy_slot, black_box(byte)) });
        }
    }
}
