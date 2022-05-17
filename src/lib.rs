//! # Diplomatic Bag
//! A mechanism for dealing with [`!Send`][`Send`] types when you really need
//! them to be [`Send`].
//!
//! This library provides the [`DiplomaticBag<T>`] type that is [`Send`] and
//! [`Sync`] even if the type it wraps is not. It does this by preventing direct
//! access to the wrapped type but instead provides methods for interacting with
//! it on a thread that it never leaves.
//!
//! This is useful for when you have a [`!Send`][`Send`] type (usually an FFI
//! type) that you need store for a long period of time, and needs to be
//! accessible from multiple threads, for example, in async code.
//!
//! # Examples
//! ```
//! # use diplomatic_bag::DiplomaticBag;
//! # use std::{cell::RefCell, rc::Rc};
//! // `Rc` is neither `Send` nor `Sync`
//! let foo = DiplomaticBag::new(|_| Rc::new(RefCell::new(0)));
//!
//! std::thread::spawn({
//!     let foo = foo.clone();
//!     move || {
//!         foo.as_ref().map(|_, rc| {
//!             *rc.borrow_mut() = 1;
//!         });
//!     }
//! });
//! ```
//! Now, being able to send an `Rc` around isn't very useful, but this comes in
//! handy when dealing with FFI types that must remain on the same thread for
//! their existence.

#![doc(html_root_url = "https://docs.rs/diplomatic-bag/0.3.1")]
#![warn(
    keyword_idents,
    missing_crate_level_docs,
    missing_debug_implementations,
    missing_docs,
    non_ascii_idents
)]
#![forbid(unsafe_op_in_unsafe_fn)]

use crossbeam_channel::{bounded, unbounded, Sender};
use once_cell::sync::Lazy;
use std::{
    fmt,
    marker::PhantomData,
    mem::{self, ManuallyDrop},
    ptr,
    thread::ThreadId,
};

/// The (sender for) the thread that all the values live on. This is lazily
/// created when the first `DiplomaticBag` is created, but will never shut down.
static THREAD_SENDER: Lazy<(Sender<Message>, ThreadId)> = Lazy::new(|| {
    let (sender, receiver) = unbounded::<Message>();
    let thread = std::thread::spawn({
        move || {
            while let Ok(closure) = receiver.recv() {
                closure();
            }
        }
    });
    (sender, thread.thread().id())
});

/// A wrapper around a `T` that always implements [`Send`] and [`Sync`], but
/// doesn't allow direct access to it's internals.
///
/// For example, this doesn't compile:
/// ```compile_fail
/// let mut foo = 0;
/// // `*mut T` doesn't implement `Send` or `Sync`
/// let bar = (&mut foo) as *mut ();
/// std::thread::spawn(|| bar);
/// ```
/// but this will:
/// ```
/// # use diplomatic_bag::DiplomaticBag;
/// let mut foo = ();
/// // `*mut T` doesn't implement `Send` or `Sync`,
/// // but `DiplomaticBag<*mut T>` does.
/// let bar = DiplomaticBag::new(|_| (&mut foo) as *mut ());
/// std::thread::spawn(|| bar);
/// ```
///
/// # Panics
/// All `DiplomaticBag`s share the same underlying thread, so if any panic, then
/// every bag immediately becomes unusable, and no new bags are able to be
/// created. This also means that the destructors of every value alive at that
/// point will never be run, potentially leaking some resources. Most of the
/// functions on a bag will panic if the underlying thread has stopped for any
/// reason.
///
/// # Blocking
/// Another consequence of every bag using the same thread is that no two values
/// can be modified concurrently, essentially sharing a lock on the thread. In
/// general, you should be protected from this; [`run`] and all the methods on
/// this type will detect if they are being run on the worker thread and will
/// avoid "taking the lock", alternatively you can use the [`BaggageHandler`] to
/// wrap and unwrap diplomatic bags safely.
/// You should aim to do as little computation as possible inside the closures
/// you provide to the functions on this type to prevent your code from blocking
/// the progress of others. All functions block until they have completed
/// executing the closure on the worker thread.
pub struct DiplomaticBag<T> {
    /// The actual value we are storing, wrapped in an [`Untouchable<T>`] so
    /// that we don't accidentally run code on it, for example drop code.
    value: Untouchable<T>,
}

// SAFETY: This is the whole point of the library, Send and Sync are safe to
// implement because accessing the variable held by the DiplomaticBag is unsafe.
unsafe impl<T> Send for DiplomaticBag<T> {}
unsafe impl<T> Sync for DiplomaticBag<T> {}

impl<T> DiplomaticBag<T> {
    /// Create a new `DiplomaticBag` by invoking the provided closure and
    /// wrapping up the value that it produces. For why you would want to do
    /// this look at the type-level or crate-level docs.
    pub fn new<F>(f: F) -> Self
    where
        F: FnOnce(BaggageHandler) -> T,
        F: Send,
    {
        run(|handler| DiplomaticBag {
            value: Untouchable::new(f(handler)),
        })
    }

    /// Maps a `DiplomaticBag<T>` to a `DiplomaticBag<U>`, by running a closure
    /// on the wrapped value.
    ///
    /// This closure must be [`Send`] as it will run on a worker thread. It
    /// should also not panic, if it does all other active bags will leak, see
    /// the type-level docs for more information.
    ///
    /// If you need to access the contents of multiple bags simultaneously, you can
    /// use the provided [`BaggageHandler`], alternatively see the
    /// [`zip()`][Self::zip()] method.
    ///
    /// # Panics
    /// This function will panic if there is an issue with the underlying worker
    /// thread, which is usually caused by this or another bag panicking.
    ///
    /// # Examples
    /// ```
    /// # use diplomatic_bag::DiplomaticBag;
    /// # use std::{cell::RefCell, rc::Rc};
    /// let foo = DiplomaticBag::new(|_| Rc::new(RefCell::new(5)));
    /// let five = foo.map(|_, foo| {
    ///     Rc::try_unwrap(foo).unwrap().into_inner()
    /// });
    /// # assert_eq!(5, five.into_inner());
    /// ```
    pub fn map<U, F>(self, f: F) -> DiplomaticBag<U>
    where
        F: FnOnce(BaggageHandler, T) -> U,
        F: Send,
    {
        run(move |handler| {
            // Safety:
            // `into_inner` can only be called on the worker thread, as it gives
            // access to a value of type `T` and `T` isn't necessarily `Send`,
            // however that's where this will run.
            let value = unsafe { self.into_inner_unchecked() };
            DiplomaticBag {
                value: Untouchable::new(f(handler, value)),
            }
        })
    }

    /// Maps a `DiplomaticBag<T>` to a `U`, by running a closure on the wrapped
    /// value.
    ///
    /// This closure must be [`Send`] as it will run on a worker thread. It
    /// should also not panic, if it does all other active bags will leak, see
    /// the type-level docs for more information.
    ///
    /// This function is especially useful for mapping over wrapper types like
    /// `Vec`, `Result`, `Option` (although see [`transpose()`][Self::transpose()]
    /// for those last two). It allows this by giving a [`BaggageHandler`] to
    /// the closure, which provides methods to wrap and unwrap `DiplomaticBag`s.
    ///
    /// # Panics
    /// This function will panic if there is an issue with the underlying worker
    /// thread, which is usually caused by this or another bag panicking.
    ///
    /// # Examples
    /// ```
    /// # use diplomatic_bag::DiplomaticBag;
    /// let foo = DiplomaticBag::new(|_| vec!["ho"; 3]);
    /// let foo: Vec<_> = foo.and_then(|handler, foo| {
    ///     foo.into_iter().map(|item| handler.wrap(item)).collect()
    /// });
    /// # assert_eq!(&"ho", foo[0].as_ref().into_inner());
    /// ```
    pub fn and_then<U, F>(self, f: F) -> U
    where
        U: Send,
        F: FnOnce(BaggageHandler, T) -> U,
        F: Send,
    {
        run(move |handler| {
            // Safety:
            // `into_inner` can only be called on the worker thread, as it gives
            // access to a value of type `T` and `T` isn't necessarily `Send`,
            // however that's where this will run.
            let value = unsafe { self.into_inner_unchecked() };
            f(handler, value)
        })
    }

    /// Combine a `DiplomaticBag<T>` and a `DiplomaticBag<U>` into a
    /// `DiplomaticBag<(T, U)>`.
    ///
    /// This is useful when combined with [`map()`][Self::map()] to allow
    /// interacting with the internals of multiple bags simultaneously.
    ///
    /// # Examples
    /// ```
    /// # use diplomatic_bag::DiplomaticBag;
    /// let one = DiplomaticBag::new(|_| 1);
    /// let two = DiplomaticBag::new(|_| 2);
    /// let three = one.zip(two).map(|_, (one, two)| one + two);
    /// # assert_eq!(3, three.into_inner());
    /// ```
    pub fn zip<U>(self, other: DiplomaticBag<U>) -> DiplomaticBag<(T, U)> {
        // Safety:
        // We immediately wrap up the values returned by `into_inner_unchecked`
        // so they spend the minimum amount of time on this thread. The only
        // danger here is them accidentally getting dropped on this thread, but
        // none of these functions can panic.
        let value = unsafe {
            Untouchable::new((self.into_inner_unchecked(), other.into_inner_unchecked()))
        };
        DiplomaticBag { value }
    }

    /// Converts a `&DiplomaticBag<T>` into a `DiplomaticBag<&T>`.
    ///
    /// # Examples
    /// ```
    /// # use diplomatic_bag::DiplomaticBag;
    /// let a = DiplomaticBag::new(|_| 0);
    /// let b = a.as_ref().map(|_, a| a.clone());
    /// # assert_eq!(a, b);
    /// ```
    pub fn as_ref(&self) -> DiplomaticBag<&T> {
        // Safety:
        // `as_ref` produces a `&T`, which is not necessarily `Send` as `T` may
        // not be `Sync`. However it is immediately wrapped in an `Untouchable`
        // again and `&T`s are only read explicitly.
        let value = unsafe { Untouchable::new(self.value.as_ref()) };
        DiplomaticBag { value }
    }

    /// Converts a `&mut DiplomaticBag<T>` into a `DiplomaticBag<&mut T>`.
    ///
    /// # Examples
    /// ```
    /// # use diplomatic_bag::DiplomaticBag;
    /// let mut a = DiplomaticBag::new(|_| 1);
    /// let mut b = DiplomaticBag::new(|_| 2);
    /// a.as_mut().zip(b.as_mut()).map(|_, (a, b)| {
    ///     std::mem::swap(a, b);
    /// });
    /// # assert_eq!(2, a.into_inner());
    /// # assert_eq!(1, b.into_inner());
    /// ```
    pub fn as_mut(&mut self) -> DiplomaticBag<&mut T> {
        // Safety:
        // `as_mut` produces a `&mut T`, which is not necessarily `Send` as `T`
        // may not be `Send`. However it is immediately wrapped in an
        // `Untouchable` again and `&mut T`s are only read explicitly.
        let value = unsafe { Untouchable::new(self.value.as_mut()) };
        DiplomaticBag { value }
    }

    /// Unwrap the `DiplomaticBag` and retrieve the inner value.
    ///
    /// # Safety
    /// This must only be called from the worker thread if `T` is not
    /// `Send` as it was created on that thread.
    unsafe fn into_inner_unchecked(self) -> T {
        // Unfortunately you can't destructure `Drop` types at the moment, this
        // is the current workaround: pull all the fields out and then forget
        // the outer struct so the drop code isn't run. Note that the memory is
        // still freed as the bag was moved into this function, but every field
        // should be read out so that all of their destructors are run.
        let value = unsafe { ptr::read(&self.value) };
        mem::forget(self);
        // Safety: We forward these safety requirements to our caller.
        unsafe { value.into_inner() }
    }
}

impl<T: Send> DiplomaticBag<T> {
    /// Unwrap a value in a [`DiplomaticBag<T>`], allowing it to be used on this
    /// thread.
    ///
    /// This is only possible if `T` is [`Send`], as otherwise accessing the
    /// value on an arbitrary thread is UB. However, if `T` is [`Sync`] then you
    /// can run the following to obtain a `&T`.
    /// ```
    /// # fn foo<T: Sync>(bag: &diplomatic_bag::DiplomaticBag<T>) -> &T {
    /// bag.as_ref().into_inner()
    /// # }
    /// ```
    ///
    /// # Examples
    /// ```
    /// # use diplomatic_bag::DiplomaticBag;
    /// let one = DiplomaticBag::new(|_| 1);
    /// let two = DiplomaticBag::new(|_| 2);
    /// let eq = one.zip(two).map(|_, (one, two)| one == two).into_inner();
    /// # assert!(!eq);
    /// ```
    pub fn into_inner(self) -> T {
        unsafe { self.into_inner_unchecked() }
    }
}

impl<T, E> DiplomaticBag<Result<T, E>> {
    /// Convert a `DiplomaticBag<Result<T, E>>` into a
    /// `Result<DiplomaticBag<T>, DiplomaticBag<E>>`.
    ///
    /// # Examples
    /// ```
    /// # use diplomatic_bag::DiplomaticBag;
    /// fn foo() -> Result<DiplomaticBag<()>, String> {
    ///     let bag = DiplomaticBag::new(|_| {
    ///         Ok(())
    ///     });
    ///     bag.transpose().map_err(|err| err.into_inner())
    /// }
    /// ```
    pub fn transpose(self) -> Result<DiplomaticBag<T>, DiplomaticBag<E>> {
        // Safety:
        // Unclear, this isn't safe by the letter of the law as we read the
        // bytes of inner to determine if it's `Ok` or `Err`. However, although
        // not yet defined, `Send` and `Sync` are probably safety invariants,
        // not validity invariants. This means we can violate the contracts in
        // unsafe code soundly, as long as the T is never exposed on a different
        // thread to (uncontrolled) safe code.
        //
        // This only reads whether this is `Ok` or `Err` and this method cannot
        // panic, so no code that uses the thread invariant can observe T or E.
        let inner = unsafe { self.into_inner_unchecked() };
        match inner {
            Ok(val) => Ok(DiplomaticBag {
                value: Untouchable::new(val),
            }),
            Err(err) => Err(DiplomaticBag {
                value: Untouchable::new(err),
            }),
        }
    }
}

impl<T> DiplomaticBag<Option<T>> {
    /// Convert a `DiplomaticBag<Option<T>>` into a
    /// `Option<DiplomaticBag<T>>`.
    ///
    /// # Examples
    /// ```
    /// # use diplomatic_bag::DiplomaticBag;
    /// DiplomaticBag::new(|_| Some(())).transpose().unwrap_or_default();
    /// ```
    pub fn transpose(self) -> Option<DiplomaticBag<T>> {
        // Safety:
        // Same as above `transpose` for `DiplomaticBag<Result<T, E>>`.
        let inner = unsafe { self.into_inner_unchecked() };
        inner.map(|val| DiplomaticBag {
            value: Untouchable::new(val),
        })
    }
}

/// `Drop` the inner type when the `DiplomaticBag` is dropped.
///
/// Ideally, this would only be implemented when `T` is `Drop` but `Drop` must
/// be implemented for all specializations of a generic type or none. However,
/// it does use [`std::mem::needs_drop`] to identify if the drop code needs to
/// be run.
impl<T> Drop for DiplomaticBag<T> {
    fn drop(&mut self) {
        if !std::mem::needs_drop::<T>() {
            // If no drop code needs to be run for `T` we are fine to simply
            // deallocate it.
            return;
        }
        let _ = try_run(|_| {
            // Safety:
            // The inner value must only be accessed from the worker thread, and
            // that is where this closure will run.
            unsafe {
                Untouchable::drop(&mut self.value);
            }
        });
    }
}

/// This `Debug` impl currently won't forward any formatting specifiers except
/// for the `alternate` specifier (`#`).
impl<T: fmt::Debug> fmt::Debug for DiplomaticBag<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        struct AsIs(String);

        impl fmt::Debug for AsIs {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        let alt = f.alternate();

        // Basically, run Debug on the worker thread, then send the resulting String back
        let res = self
            .as_ref()
            .map(move |_, this| {
                if alt {
                    format!("{:#?}", this)
                } else {
                    format!("{:?}", this)
                }
            })
            .into_inner();

        let res = AsIs(res); // So that we don't format it with quotes, etc.

        f.debug_struct("DiplomaticBag")
            .field("inner", &res)
            .finish()
    }
}

// We can, however, implement a bunch of other useful standard traits, as below.
// Annoyingly, `serde::Serialize`, and `serde::Deserialize` both suffer from a worse
// problem than `Debug` does. They have over 30 methods that would each need to
// syncronise with the worker thread, rather than just a single result that comes
// out the end. `Display` also has the same problem, and since it is more intended
// for non-technical users, an implementation that doesn't forward all the formatting
// parameters would be worse. `From`, and `TryFrom` fail due to the orphan rules.
// `AsRef` and `AsMut` can be implemented but conflict with the existing `as_ref`
// and `as_mut` methods, and I'm not sure the confusion is worth it.

impl<T: Default> Default for DiplomaticBag<T> {
    fn default() -> Self {
        DiplomaticBag::new(|_| T::default())
    }
}

impl<T: Clone> Clone for DiplomaticBag<T> {
    fn clone(&self) -> Self {
        self.as_ref().map(|_, val| T::clone(val))
    }
}

impl<T: PartialEq> PartialEq for DiplomaticBag<T> {
    fn eq(&self, other: &Self) -> bool {
        self.as_ref()
            .zip(other.as_ref())
            .map(|_, (this, other)| T::eq(this, other))
            .into_inner()
    }
}
impl<T: Eq> Eq for DiplomaticBag<T> {}

impl<T: PartialOrd> PartialOrd for DiplomaticBag<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.as_ref()
            .zip(other.as_ref())
            .map(|_, (this, other)| T::partial_cmp(this, other))
            .into_inner()
    }
}
impl<T: Ord> Ord for DiplomaticBag<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_ref()
            .zip(other.as_ref())
            .map(|_, (this, other)| T::cmp(this, other))
            .into_inner()
    }
}

/// A type that allows wrapping and unwrapping [`DiplomaticBag`]s inside the
/// execution context of another bag.
///
/// This allows computations on the wrapped values of multiple bags, and
/// provides a mechanism for returning `!Send` and `!Sync` types from
/// computations done on values inside bags.
///
/// # Examples
/// ```
/// # use diplomatic_bag::DiplomaticBag;
/// let one = DiplomaticBag::new(|_handler| 1);
/// let two = DiplomaticBag::new(|_handler| 2);
/// let three = one.and_then(|handler, one| {
///     let three = one + handler.unwrap(two.as_ref());
///     handler.wrap(three)
/// });
/// # assert_eq!(3, three.into_inner());
#[derive(Debug, Clone, Copy)]
pub struct BaggageHandler(PhantomData<*mut ()>);

impl BaggageHandler {
    /// Create a new `BaggageHandler`.
    ///
    /// # Safety
    /// This must only be called from the worker thread, as it allows safe
    /// wrapping and unwrapping of `DiplomaticBag`s.
    unsafe fn new() -> Self {
        Self(PhantomData)
    }

    /// Wrap a value in a [`DiplomaticBag`], allowing it to be sent to other
    /// threads even if it is not `Send`.
    ///
    /// # Examples
    /// ```
    /// # use diplomatic_bag::DiplomaticBag;
    /// let foo: DiplomaticBag<u8> = DiplomaticBag::new(|_handler| 2);
    /// let bar: DiplomaticBag<u8> =
    ///     foo.and_then(|handler, value| handler.wrap(value.clone()));
    /// ```
    pub fn wrap<T>(&self, value: T) -> DiplomaticBag<T> {
        DiplomaticBag {
            value: Untouchable::new(value),
        }
    }
    /// Unwrap a value in a [`DiplomaticBag`], allowing it to be used inside
    /// the execution context of another bag.
    ///
    /// # Examples
    /// ```
    /// # use diplomatic_bag::DiplomaticBag;
    /// let one = DiplomaticBag::new(|_handler| 1);
    /// let two = DiplomaticBag::new(|_handler| 2);
    /// let three = one.and_then(|handler, one| one + handler.unwrap(two));
    /// # assert_eq!(3, three);
    /// ```
    pub fn unwrap<T>(&self, proxy: DiplomaticBag<T>) -> T {
        unsafe { proxy.into_inner_unchecked() }
    }
}

/// Run an arbitrary closure on the shared worker thread.
///
/// # Panics
/// This panics if the provided closure fails or the worker thread is not
/// running, which will happen if a previous operation panicked.
pub fn run<R, F>(f: F) -> R
where
    R: Send,
    F: FnOnce(BaggageHandler) -> R,
    F: Send,
{
    try_run(f).unwrap()
}

/// Run an arbitrary closure on the shared worker thread, similar to the
/// [`run()`][Self::run()] method. However, this does _not_ panic if the
/// worker thread has stopped unlike [`run`].
///
/// # Errors
/// This will throw an error if the operation fails, usually due to an issue
/// with the worker thread. See the [`Error`] type for more details.
fn try_run<R, F>(f: F) -> Result<R, Error>
where
    R: Send,
    F: FnOnce(BaggageHandler) -> R,
    F: Send,
{
    let (task_sender, worker) = &*THREAD_SENDER;

    if std::thread::current().id() == *worker {
        // If the current thread is the worker thread then run the closure.
        // This (mostly) prevents a deadlock where a closure on the worker
        // thread is waiting for another operation in the worker threads queue.

        // Safety: we are on the worker thread.
        let baggage_handler = unsafe { BaggageHandler::new() };
        return Ok(f(baggage_handler));
    }

    // Set up a rendezvous channel so that the closure can return the value.
    let (sender, receiver) = bounded(0);
    // This is the closure that will get run on the worker thread, it gets
    // boxed up as we're passing ownership to that thread and we can't pass
    // it directly due to every invocation of this function potentially
    // having a different closure type.
    let closure = Box::new(move || {
        let baggage_handler = unsafe { BaggageHandler::new() };
        let value = f(baggage_handler);
        let _ = sender.send(value);
        // Note that after calling `send` we have dropped or returned all values
        // that could possibly be holding references to data on the calling
        // thread, which makes it safe for `try_run` to return.
    }) as Box<dyn FnOnce() + Send>;

    // Extend the closure's lifetime as rust doesn't know that we won't return
    // until the closure has given all possible references back to us.
    // Safety:
    // We have to be careful with this closure from now on but we know that
    // anything borrowed by the closure must live at least as long as this
    // function call. So we must be careful that this closure is dropped before
    // this function returns and `result` is dropped.
    let closure: Box<dyn FnOnce() + Send + 'static> = unsafe { mem::transmute(closure) };

    // Send the closure to the worker thread!
    // `task_sender` is an unbounded channel so this shouldn't block but
    // it may fail if the worker thread has stopped. In that case the
    // message gets given back to us and immediately dropped, satisfying the
    // closure safety conditions.
    task_sender.send(closure).map_err(|_| Error::Send)?;
    // The closure is now running/pending on the worker thread. It will notify
    // us when it's done and in the meantime we must keep everything alive.
    // Note that `recv` can fail, but only in the case where the channel gets
    // disconnected and the only way that can happen is if the worker thread
    // drops the message, so we're safe to exit.
    receiver.recv().map_err(|_| Error::Recv)
}

/// The error type used by [`DiplomaticBag<T>::try_run()`].
///
/// This indicates that the underlying worker thread is not running, this is
/// probably because a user provided closure panicked and crashed the thread.
///
/// [`DiplomaticBag<T>::try_run()`]: DiplomaticBag::try_run()
#[derive(Debug)]
enum Error {
    /// An issue occurred with sending the closure to the worker thread.
    /// This would usually indicate that the worker thread has stopped for some
    /// reason (presumably a user provided closure panicked).
    Send,
    /// An issue occurred while waiting for the worker thread to send the
    /// notification back, either the closure panicked or something in the
    /// queue before us panicked.
    Recv,
}

impl std::error::Error for Error {}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("diplomatic bag worker thread not running")
    }
}

/// The code to run on the worker thread.
type Message = Box<dyn FnOnce() + Send + 'static>;

/// A wrapper type that makes it completely unsafe to access the type that it
/// wraps. This is what makes the [`DiplomaticBag`] type `Send` and `Sync` even
/// when `T` is not. It has similar semantics to [`ManuallyDrop`] as it just
/// wraps one.
#[repr(transparent)]
struct Untouchable<T>(ManuallyDrop<T>);

impl<T> Untouchable<T> {
    /// Create a new `Untouchable`.
    fn new(value: T) -> Self {
        Self(ManuallyDrop::new(value))
    }

    /// Consume the `Untouchable` and get the wrapped type out.
    ///
    /// # Safety
    /// This must be called on the same thread that the type was created on if
    /// `T` is not `Send`.
    unsafe fn into_inner(self) -> T {
        ManuallyDrop::into_inner(self.0)
    }

    /// Get a shared reference to the wrapped type.
    ///
    /// # Safety
    /// This must be called on the same thread that the type was created on if
    /// `T` is not `Sync`.
    unsafe fn as_ref(&self) -> &T {
        &self.0
    }

    /// Get a shared reference to the wrapped type.
    ///
    /// # Safety
    /// This must be called on the same thread that the type was created on if
    /// `T` is not `Send`.
    unsafe fn as_mut(&mut self) -> &mut T {
        &mut self.0
    }

    /// Runs the drop code on the wrapped value.
    ///
    /// # Safety
    /// This must be called on the same thread that the type was created on if
    /// `T` is not `Send`. It must also only ever be called once, and the value
    /// inside the `Untouchable` never accessed again. Preferably, the
    /// `Untouchable` should be immediately dropped after calling this method.
    unsafe fn drop(&mut self) {
        unsafe { ManuallyDrop::drop(&mut self.0) };
    }
}

// Safety:
// It is unsafe to access the value inside an `Untouchable`, so it's ok for the
// wrapper to be `Send` and `Sync`.
unsafe impl<T> Send for Untouchable<T> {}
unsafe impl<T> Sync for Untouchable<T> {}

#[cfg(test)]
mod tests {
    use slotmap::{DefaultKey, SlotMap};
    use static_assertions::{assert_impl_all, assert_not_impl_any};
    use std::{
        cell::{Cell, RefCell},
        marker::PhantomData,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    };

    use super::*;

    thread_local! {
        static TEST: RefCell<SlotMap<DefaultKey, u32>> = RefCell::new(SlotMap::new());
    }

    struct NotSend {
        key: DefaultKey,
        value: Cell<u32>,
        marker: PhantomData<*mut ()>,
    }
    impl NotSend {
        fn new() -> Self {
            let value = rand::random();
            let key = TEST.with(|map| map.borrow_mut().insert(value));
            Self {
                key,
                value: Cell::new(value),
                marker: PhantomData,
            }
        }

        fn change(&self) {
            self.value.set(rand::random());
            TEST.with(|map| map.borrow_mut()[self.key] = self.value.get())
        }

        fn verify(&self) {
            assert_eq!(
                Some(self.value.get()),
                TEST.with(|map| map.borrow().get(self.key).copied())
            );
        }
    }
    impl Drop for NotSend {
        fn drop(&mut self) {
            self.verify()
        }
    }

    assert_impl_all!(DiplomaticBag<*mut ()>: Send, Sync);
    assert_not_impl_any!(BaggageHandler: Send, Sync);
    assert_impl_all!(Error: std::error::Error, Send, Sync);

    #[test]
    fn create_and_drop() {
        let _value = DiplomaticBag::new(|_| NotSend::new());
    }

    #[test]
    fn execute() {
        let value = DiplomaticBag::new(|_| NotSend::new());
        value.map(|_, value| value.verify());
    }

    #[test]
    fn execute_ref() {
        let value = DiplomaticBag::new(|_| NotSend::new());
        value.as_ref().map(|_, value| {
            value.verify();
            value.change();
        });
    }

    #[test]
    fn execute_mut() {
        let mut value = DiplomaticBag::new(|_| NotSend::new());
        value.as_mut().map(|_, value| {
            value.verify();
            value.change();
        });
    }

    #[test]
    fn drop_inner() {
        let atomic = Arc::new(AtomicBool::new(false));
        struct SetOnDrop(Arc<AtomicBool>);
        impl Drop for SetOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let bag = DiplomaticBag::new(|_| SetOnDrop(atomic.clone()));
        assert!(!atomic.load(Ordering::SeqCst));
        drop(bag);
        assert!(atomic.load(Ordering::SeqCst));
    }

    #[test]
    fn readme_version() {
        version_sync::assert_markdown_deps_updated!("README.md");
    }

    #[test]
    fn html_root_url_version() {
        version_sync::assert_html_root_url_updated!("src/lib.rs");
    }
}
