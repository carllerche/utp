/// Safely generates two sequential connection identifiers.
///
/// This avoids an overflow when the generated receiver identifier is the largest
/// representable value in u16 and it is incremented to yield the corresponding sender
/// identifier.
pub fn generate_sequential_identifiers() -> (u16, u16) {
    let id = next_u16();

    if id.checked_add(1).is_some() {
        (id, id + 1)
    } else {
        (id - 1, id)
    }
}

#[cfg(not(test))]
fn next_u16() -> u16 {
    use rand::{self, Rng};

    let mut rng = rand::thread_rng();
    rng.gen::<u16>()
}

#[cfg(test)]
fn next_u16() -> u16 {
    use rand::{XorShiftRng, Rng};
    use std::cell::RefCell;

    thread_local!(static THREAD_RNG: RefCell<XorShiftRng> = {
        RefCell::new(XorShiftRng::new_unseeded())
    });

    THREAD_RNG.with(|t| t.borrow_mut().gen::<u16>())
}
