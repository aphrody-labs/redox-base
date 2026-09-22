//! Assertion and test registration macros.
//!
//! Macros return early with [`Outcome::Fail`] on assertion failures, stopping the
//! test immediately.
//!
//! [`Outcome::Fail`]: crate::Outcome::Fail

/// Asserts that a condition is true, returning [`Outcome::Fail`] with a formatted message if false.
///
/// [`Outcome::Fail`]: crate::Outcome::Fail
#[macro_export]
macro_rules! check {
    ($cond:expr, $($arg:tt)*) => {
        if !($cond) {
            return $crate::Outcome::Fail(format!($($arg)*));
        }
    };
}

/// Asserts that two expressions are equal, returning [`Outcome::Fail`] on mismatch.
///
/// Both operands are taken by reference (`&$left`, `&$right`). For comparing temporary
/// buffers or strings, prefer [`check_bytes!`] or [`check_str!`] to avoid lifetime issues.
///
/// [`Outcome::Fail`]: crate::Outcome::Fail
/// [`check_bytes!`]: crate::check_bytes
/// [`check_str!`]: crate::check_str
#[macro_export]
macro_rules! check_eq {
    ($left:expr, $right:expr) => {{
        let left = &$left;
        let right = &$right;
        if left != right {
            return $crate::Outcome::Fail(format!(
                "assertion failed: `(left == right)`\n  left: `{left:?}`\n right: `{right:?}`"
            ));
        }
    }};
    ($left:expr, $right:expr, $($arg:tt)*) => {{
        let left = &$left;
        let right = &$right;
        if left != right {
            return $crate::Outcome::Fail(format!(
                "{}: left=`{left:?}` right=`{right:?}`",
                format!($($arg)*),
            ));
        }
    }};
}

/// Unwraps an `Ok(v)` from a `Result`, returning early with [`Outcome::Fail`] on `Err`.
///
/// For helpers returning `Result<T, Outcome>`, use [`check_out!`] to preserve [`Outcome::Skip`].
///
/// [`Outcome::Fail`]: crate::Outcome::Fail
/// [`Outcome::Skip`]: crate::Outcome::Skip
/// [`check_out!`]: crate::check_out
#[macro_export]
macro_rules! check_ok {
    ($expr:expr) => {
        match $expr {
            Ok(v) => v,
            Err(e) => {
                return $crate::Outcome::Fail(format!("{}: {e}", stringify!($expr)));
            }
        }
    };
    ($expr:expr, $($arg:tt)*) => {
        match $expr {
            Ok(v) => v,
            Err(e) => {
                return $crate::Outcome::Fail(format!("{}: {e}", format!($($arg)*)));
            }
        }
    };
}

/// Asserts that an expression evaluates to `Err`, returning early with [`Outcome::Fail`] on `Ok`.
///
/// Evaluates to the unwrapped error on success.
///
/// [`Outcome::Fail`]: crate::Outcome::Fail
#[macro_export]
macro_rules! check_err {
    ($expr:expr, $($arg:tt)*) => {
        match $expr {
            Err(e) => e,
            Ok(_) => {
                return $crate::Outcome::Fail(format!(
                    "{}: expected error, succeeded",
                    format!($($arg)*)
                ));
            }
        }
    };
}

/// Internal helper for [`check_bytes!`] and [`check_str!`].
///
/// [`check_bytes!`]: crate::check_bytes
/// [`check_str!`]: crate::check_str
#[doc(hidden)]
#[macro_export]
macro_rules! check_via {
    ($acc:ident, $kind:literal, $got:expr, $want:expr) => {{
        let got = $got;
        let want = &$want;
        if got.$acc() != *want {
            return $crate::Outcome::Fail(format!(
                "{} mismatch\n  left: `{got:?}`\n right: `{want:?}`",
                $kind
            ));
        }
    }};
    ($acc:ident, $got:expr, $want:expr, $($arg:tt)*) => {{
        let got = $got;
        let want = &$want;
        if got.$acc() != *want {
            return $crate::Outcome::Fail(format!(
                "{}: left=`{got:?}` right=`{want:?}`",
                format!($($arg)*)
            ));
        }
    }};
}

/// Compares a byte sequence (`as_slice()`) against a slice (`&[u8]`).
///
/// Takes ownership of `$got` before slicing, allowing comparisons on temporary values.
#[macro_export]
macro_rules! check_bytes {
    ($got:expr, $want:expr) => {
        $crate::check_via!(as_slice, "bytes", $got, $want)
    };
    ($got:expr, $want:expr, $($arg:tt)*) => {
        $crate::check_via!(as_slice, $got, $want, $($arg)*)
    };
}

/// Compares a string (`as_str()`) against a string slice (`&str`).
///
/// Takes ownership of `$got` before borrowing, allowing comparisons on temporary values.
#[macro_export]
macro_rules! check_str {
    ($got:expr, $want:expr) => {
        $crate::check_via!(as_str, "string", $got, $want)
    };
    ($got:expr, $want:expr, $($arg:tt)*) => {
        $crate::check_via!(as_str, $got, $want, $($arg)*)
    };
}

/// Asserts that a `Result` is an error with the expected OS error code (`raw_os_error`).
///
/// Returns the error value on match, or returns [`Outcome::Fail`] on mismatch or success.
///
/// [`Outcome::Fail`]: crate::Outcome::Fail
#[macro_export]
macro_rules! check_errno {
    ($expr:expr, $want:expr, $($arg:tt)*) => {{
        let err = $crate::check_err!($expr, $($arg)*);
        if err.raw_os_error() != Some($want) {
            return $crate::Outcome::Fail(format!(
                "{}: expected errno {}, got {err:?}",
                format!($($arg)*),
                $want
            ));
        }
        err
    }};
}

/// Asserts that a `Result` is an error matching any of the specified OS error codes.
///
/// Useful when multiple error codes are valid under POSIX or platform implementations.
/// Returns the error value on match, or returns [`Outcome::Fail`] on mismatch or success.
///
/// [`Outcome::Fail`]: crate::Outcome::Fail
#[macro_export]
macro_rules! check_errno_any {
    ($expr:expr, [$($want:expr),+ $(,)?], $($arg:tt)*) => {{
        let err = $crate::check_err!($expr, $($arg)*);
        let want = [$($want),+];
        if !err.raw_os_error().is_some_and(|e| want.contains(&e)) {
            return $crate::Outcome::Fail(format!(
                "{}: expected errno {want:?}, got {err:?}",
                format!($($arg)*)
            ));
        }
        err
    }};
}

/// Unwraps a `Result<T, Outcome>`, returning `T` on `Ok` or early-returning the `Outcome` on `Err`.
///
/// Unlike [`check_ok!`], this propagates non-failure outcomes such as [`Outcome::Skip`].
///
/// [`Outcome::Skip`]: crate::Outcome::Skip
/// [`check_ok!`]: crate::check_ok
#[macro_export]
macro_rules! check_out {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(o) => return o,
        }
    };
}

/// Builds a list of test cases for suite registration.
///
/// # Syntax
/// - `cases![func name, edge other_name]`: Registers functions `name(&ctx)` and `other_name(&ctx)`.
/// - `cases![func name => |ctx| expr]`: Registers a custom closure or helper call under `name`.
///
/// Generates calls to `$crate::func`, `$crate::edge`, or `$crate::perf`.
#[macro_export]
macro_rules! cases {
    ($($kind:ident $name:ident $(=> $ctor:expr)?),+ $(,)?) => {
        vec![
            $($crate::cases!(@one $kind $name $(=> $ctor)?),)+
        ]
    };
    (@one $kind:ident $name:ident => $ctor:expr) => {
        $crate::$kind(stringify!($name), $ctor)
    };
    (@one $kind:ident $name:ident) => {
        $crate::$kind(stringify!($name), $name)
    };
}
