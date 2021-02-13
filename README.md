# Diplomatic Bag
[![Crates.io](https://img.shields.io/crates/v/diplomatic-bag.svg)](https://crates.io/crates/diplomatic-bag)
[![API reference](https://docs.rs/diplomatic-bag/badge.svg)](https://docs.rs/diplomatic-bag/)
[![License](https://img.shields.io/badge/license-MIT_OR_Apache--2.0-blue.svg)](
https://github.com/Skepfyr/DiplomaticBag#license)
[![Rust](https://github.com/Skepfyr/DiplomaticBag/workflows/Rust/badge.svg)](https://github.com/Skepfyr/DiplomaticBag/actions?query=workflow%3ARust+branch%3Amaster)

Hide `!Send` and `!Sync` types inside a [`DiplomaticBag`](https://en.wikipedia.org/wiki/Diplomatic_bag) which can be sent between thread freely.

```rust
let one = DiplomaticBag::new(|| Rc::new(RefCell::new(1)));
let two = DiplomaticBag::new(|| Rc::new(RefCell::new(2)));
let three: u8 = std::thread::spawn(|| {
    one.as_ref()
        .zip(two.as_ref())
        .map(|(one, two)| *one.borrow() + *two.borrow())
        .into_inner()
}).join()?;
```
(I don't know why you'd want to do this ^, but I promise this comes in handy for `!Send` types not in std, for example, [cpal's `Stream` type](https://docs.rs/cpal/*/cpal/struct.Stream.html).)


## How it works

All the operations on values inside `DiplomaticBags` are sent to a worker thread (the home country 😉).
This means the values are only ever read on one thread, allowing the `DiplomaticBag` to be `Send` even if the type it contains is not.
This model does have some drawbacks, see [the docs](https://docs.rs/diplomatic-bag/) for more information.

## License

Licensed under either of

 * Apache License, Version 2.0
   ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license
   ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
