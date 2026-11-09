#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("this linker contract requires Linux x86_64");

std::arch::global_asm!(
    r#"
    .section .text.icf_pointer_a,"ax",@progbits
    .global icf_pointer_a
    .type icf_pointer_a,@function
icf_pointer_a:
    mov eax, 7
    ret
    .size icf_pointer_a, .-icf_pointer_a
    .section .text.icf_pointer_b,"ax",@progbits
    .global icf_pointer_b
    .type icf_pointer_b,@function
icf_pointer_b:
    mov eax, 7
    ret
    .size icf_pointer_b, .-icf_pointer_b
    .addrsig
    .addrsig_sym icf_pointer_a
    .addrsig_sym icf_pointer_b
"#
);

unsafe extern "C" {
    fn icf_pointer_a() -> i32;
    fn icf_pointer_b() -> i32;
}

#[test]
fn address_significant_ffi_functions_stay_distinct() {
    let a = std::hint::black_box(icf_pointer_a as unsafe extern "C" fn() -> i32);
    let b = std::hint::black_box(icf_pointer_b as unsafe extern "C" fn() -> i32);
    assert_ne!(a as usize, b as usize);
}

#[test]
fn ffi_leaf_calls_preserve_results() {
    // SAFETY: Both assembly functions use the C ABI and return a constant.
    let values = unsafe { [icf_pointer_a(), icf_pointer_b()] };
    assert_eq!(values, [7, 7]);
}

#[test]
fn unwinding_runs_drop_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountDrop<'a>(&'a AtomicUsize);
    impl Drop for CountDrop<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let drops = AtomicUsize::new(0);
    let result = std::panic::catch_unwind(|| {
        let _guard = CountDrop(&drops);
        panic!("linker unwind contract");
    });
    assert!(result.is_err() && drops.load(Ordering::SeqCst) == 1);
}

#[test]
fn trait_objects_preserve_type_and_method_identity() {
    use std::any::Any;
    trait Value: Any {
        fn value(&self) -> u64;
    }
    struct A(u64);
    struct B(u64);
    impl Value for A {
        fn value(&self) -> u64 {
            self.0
        }
    }
    impl Value for B {
        fn value(&self) -> u64 {
            self.0
        }
    }
    let a = A(7);
    let b = B(9);
    let values = std::hint::black_box([&a as &dyn Value, &b as &dyn Value]);
    assert!(values[0].type_id() != values[1].type_id());
    assert_eq!(values.map(Value::value), [7, 9]);
}
