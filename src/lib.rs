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
//! let foo = DiplomaticBag::new(|| Rc::new(RefCell::new(0)));
//!
//! std::thread::spawn({
//!     let foo = foo.clone();
//!     move || {
//!         foo.as_ref().map(|rc| {
//!             *rc.borrow_mut() = 1;
//!         });
//!     }
//! });
//! ```
//! Now, being able to send an `Rc` around isn't very useful, but this comes in
//! handy when dealing with FFI types that must remain on the same thread for
//! their existence.

#![doc(html_root_url = "https://docs.rs/diplomatic-bag/0.1.0")]
#![warn(
    keyword_idents,
    missing_crate_level_docs,
    missing_debug_implementations,
    missing_docs,
    non_ascii_idents
)]

use flume::{bounded, unbounded, Sender};
use once_cell::sync::Lazy;
use std::{
    fmt,
    mem::{self, ManuallyDrop},
    ptr,
    sync::Mutex,
};

/// The (sender for) the thread that all the values live on. This is lazily
/// created when the first `DiplomaticBag` is created, but will never shut down.
static THREAD_SENDER: Lazy<Sender<Message>> = Lazy::new(|| {
    let (sender, receiver) = unbounded::<Message>();
    std::thread::spawn({
        move || {
            while let Ok(Message { closure, response }) = receiver.recv() {
                // Run the code given to us, we need to be careful here to not
                // store anything off as the caller's safety relies on us
                // dropping everything before sending something down `response`.
                closure();
                // Notify the caller that we are done with everything that was
                // lent to us. Ignore any errors as there's nothing sensible we
                // can do.
                let _ = response.send(());
            }
        }
    });
    sender
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
/// let bar = DiplomaticBag::new(|| (&mut foo) as *mut ());
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
/// can be modified concurrently, essentially sharing a lock on the thread. You
/// should aim to do as little computation as possible inside the closures you
/// provide to the functions on this type to prevent your code from blocking
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
        F: FnOnce() -> T,
        F: Send,
    {
        run(|| DiplomaticBag {
            value: Untouchable::new(f()),
        })
    }

    /// Maps a `DiplomaticBag<T>` to a `DiplomaticBag<U>`, by running a closure
    /// on the wrapped value.
    ///
    /// This closure must be [`Send`] as it will run on a worker thread. It
    /// should also not panic, if it does all other active bags will leak, see
    /// the type-level docs for more information.
    ///
    /// If you need to access the contents of multiple bags simultaneously, look
    /// at the [`zip()`][Self::zip()] method.
    ///
    /// # Panics
    /// This function will panic if there is an issue with the underlying worker
    /// thread, which is usually caused by this or another bag panicking.
    ///
    /// # Examples
    /// ```
    /// # use diplomatic_bag::DiplomaticBag;
    /// # use std::{cell::RefCell, rc::Rc};
    /// let foo = DiplomaticBag::new(|| Rc::new(RefCell::new(5)));
    /// let five = foo.map(|foo| {
    ///     Rc::try_unwrap(foo).unwrap().into_inner()
    /// });
    /// # assert_eq!(5, five.into_inner());
    /// ```
    pub fn map<U, F>(self, f: F) -> DiplomaticBag<U>
    where
        F: FnOnce(T) -> U,
        F: Send,
    {
        run(move || {
            // Safety:
            // `into_inner` can only be called on the worker thread, as it gives
            // access to a value of type `T` and `T` isn't necessarily `Send`,
            // however that's where this will run.
            let value = unsafe { self.into_inner_unchecked() };
            DiplomaticBag {
                value: Untouchable::new(f(value)),
            }
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
    /// let one = DiplomaticBag::new(|| 1);
    /// let two = DiplomaticBag::new(|| 2);
    /// let three = one.zip(two).map(|(one, two)| one + two);
    /// # assert_eq!(3, three.into_inner());
    /// ```
    pub fn zip<U>(self, other: DiplomaticBag<U>) -> DiplomaticBag<(T, U)> {
        // Safety:
        // We immediately wrap up the values returned by `into_inner_unchecked`
        // so they spend the minimum amount of time on this thread. The only
        // danger here is them accidentally getting dropped on this thread, but
        // none of these functions can panic.
        //
        // Note, I'm not particularly happy with this as the `T` and the `U`
        // spend a worrying amount of time on this thread in a droppable form.
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
    /// let a = DiplomaticBag::new(|| 0);
    /// let b = a.as_ref().map(|a| a.clone());
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
    /// let mut a = DiplomaticBag::new(|| 1);
    /// let mut b = DiplomaticBag::new(|| 2);
    /// a.as_mut().zip(b.as_mut()).map(|(a, b)| {
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
        let value = ptr::read(&self.value);
        mem::forget(self);
        value.into_inner()
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
    /// let one = DiplomaticBag::new(|| 1);
    /// let two = DiplomaticBag::new(|| 2);
    /// let eq = one.zip(two).map(|(one, two)| one == two).into_inner();
    /// # assert!(!eq);
    /// ```
    pub fn into_inner(self) -> T {
        unsafe { self.into_inner_unchecked() }
    }
}

/// `Drop` the inner type when the `DiplomaticBag` is dropped.
///
/// Ideally, this would only be implemented when `T` is `Drop` but `Drop` must
/// be implemented for all specializations of a generic type or none.
impl<T> Drop for DiplomaticBag<T> {
    fn drop(&mut self) {
        let _ = try_run(|| {
            // Safety:
            // The inner value must only be accessed from the worker thread, and
            // that is where this closure will run.
            unsafe {
                Untouchable::drop(&mut self.value);
            }
        });
    }
}

/// Unfortunately, we can't implement `Debug` properly for `T`s that implement
/// `Debug` because the formatter isn't `Send`. Instead, we just be a bit
/// enigmatic and say we have something but refuse to say what it is.
impl<T> fmt::Debug for DiplomaticBag<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DiplomaticBag<{}> {{ .. }}", std::any::type_name::<T>())
    }
}

// We can, however, implement a bunch of other useful standard traits, as below.
// Annoyingly, `Display`, `serde::Serialize`, and `serde::Deserialize` all
// suffer from the same problem as `Debug`. Also, `AsRef`, `AsMut`, `From`, and
// `TryFrom` all fail for similar reasons.

impl<T: Default> Default for DiplomaticBag<T> {
    fn default() -> Self {
        DiplomaticBag::new(T::default)
    }
}

impl<T: Clone> Clone for DiplomaticBag<T> {
    fn clone(&self) -> Self {
        self.as_ref().map(T::clone)
    }
}

impl<T: PartialEq> PartialEq for DiplomaticBag<T> {
    fn eq(&self, other: &Self) -> bool {
        self.as_ref()
            .zip(other.as_ref())
            .map(|(this, other)| T::eq(this, other))
            .into_inner()
    }
}
impl<T: Eq> Eq for DiplomaticBag<T> {}

impl<T: PartialOrd> PartialOrd for DiplomaticBag<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.as_ref()
            .zip(other.as_ref())
            .map(|(this, other)| T::partial_cmp(this, other))
            .into_inner()
    }
}
impl<T: Ord> Ord for DiplomaticBag<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_ref()
            .zip(other.as_ref())
            .map(|(this, other)| T::cmp(this, other))
            .into_inner()
    }
}

/// Run an arbitrary closure on the shared worker thread.
///
/// # Panics
/// This panics if the operation fails for any reason, usually due to the
/// shared worker thread not running for some reason.
fn run<R, F>(f: F) -> R
where
    R: Send,
    F: FnOnce() -> R,
    F: Send,
{
    try_run(f).unwrap()
}

/// Run an arbitrary closure on the shared worker thread, similar to the
/// [`run()`][Self::run()] method. However, this does _not_ panic if the
/// worker thread has stopped unlike all the other methods on this type.
///
/// # Errors
/// This will throw an error if the operation fails, usually due to an issue
/// with the worker thread. See the [`Error`] type for more details.
fn try_run<R, F>(f: F) -> Result<R, Error>
where
    R: Send,
    F: FnOnce() -> R,
    F: Send,
{
    // Location to write the result of the computation to. In an ideal world
    // the result of the closure would come back up the response channel,
    // but that would require a lot of messing around due to types having
    // different sizes. We work around all that by writing it directly to
    // this bit of memory.
    let result = Mutex::new(Option::None);
    let result_ref = &result;
    // This is the closure that will get run on the worker thread, it gets
    // boxed up as we're passing ownership to that thread and we can't pass
    // it directly due to every invocation of this function potentially
    // having a different closure type.
    let closure = Box::new(move || {
        let value = f();
        let mut result = result_ref.lock().unwrap();
        *result = Some(value);
    }) as Box<dyn FnOnce() + Send>;
    // Extend the closure's lifetime as rust doesn't know what the thread
    // we're sending this closure to is going to do with it.
    // Safety:
    // We have to be careful with this closure from now on but we know that
    // anything borrowed by the closure must live at least as long as this
    // function call, or when `result` is invalid. So we must be careful
    // that this closure is dropped before this function returns and `result`
    // is dropped.
    let closure: Box<dyn FnOnce() + Send + 'static> = unsafe { mem::transmute(closure) };
    // Set up a rendezvous channel so that the worker thread can signal when
    // it is done with the closure.
    let (response, receiver) = bounded(0);
    // Send the closure and response channel to the worker thread!
    // `THREAD_SENDER` is an unbounded channel so this shouldn't block but
    // it may fail if the worker thread has stopped. In that case the
    // message gets given back to us and immediately dropped, satisfying the
    // closure safety conditions.
    THREAD_SENDER
        .send(Message { closure, response })
        .map_err(|_| Error::Send)?;
    // The closure is now running/pending running on the worker thread. It
    // will notify us when it's done and in the meantime we must keep
    // everything alive. Note that `recv` can fail, but only in the case
    // where the channel gets disconnected and the only way that can happen
    // is if the worker thread drops the message, so we're safe to exit.
    receiver.recv().map_err(|_| Error::Recv)?;
    let _ = result_ref;
    let result = result.into_inner().unwrap();
    // Our closure has now run successfully, so just read out and return the
    // result. In the impossible event that it hasn't been initialised throw
    // an error.
    result.ok_or(Error::NotInit)
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
    /// The result hasn't been initialised, this shouldn't be possible to hit.
    NotInit,
}

impl std::error::Error for Error {}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("diplomatic bag worker thread not running")
    }
}

/// The message to send to the worker thread, containing the code to run and
/// the channel to notify us on when it's done.
struct Message {
    /// The code to run on the worker thread, the 'static bound is a bit
    /// disingenuous as this closure only has to live until a notification is
    /// sent to the response channel.
    closure: Box<dyn FnOnce() + Send + 'static>,
    /// The mechanism to notify the caller that the worker thread has consumed
    /// the closure.
    response: Sender<()>,
}

/// A wrapper type that makes it completely unsafe to access the type that it
/// wraps. This is what makes the [`DiplomaticBag`] type `Send` and `Sync` even
/// when `T` is not. It has similar semantics to [`ManuallyDrop`] as it just
/// wraps one.
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
        ManuallyDrop::drop(&mut self.0);
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
    use static_assertions::assert_impl_all;
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
    assert_impl_all!(Error: std::error::Error, Send, Sync);

    #[test]
    fn create_and_drop() {
        let _value = DiplomaticBag::new(NotSend::new);
    }

    #[test]
    fn execute() {
        let value = DiplomaticBag::new(NotSend::new);
        value.map(|value| value.verify());
    }

    #[test]
    fn execute_ref() {
        let value = DiplomaticBag::new(NotSend::new);
        value.as_ref().map(|value| {
            value.verify();
            value.change();
        });
    }

    #[test]
    fn execute_mut() {
        let mut value = DiplomaticBag::new(NotSend::new);
        value.as_mut().map(|value| {
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

        let bag = DiplomaticBag::new(|| SetOnDrop(atomic.clone()));
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
