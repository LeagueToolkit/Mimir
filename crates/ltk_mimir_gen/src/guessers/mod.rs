//! The candidate-generation strategies. Strings mined from WAD archives
//! (see [`crate::mine`]) enter the hunt through [`SeedStrings`].

mod cross;
mod lcu;
mod numbers;
mod seeds;
mod skins;
pub(crate) mod util;
mod variants;
mod words;

pub use cross::CrossReference;
pub use lcu::RegionLocale;
pub use numbers::NumericRange;
pub use seeds::SeedStrings;
pub use skins::CharacterSkin;
pub use variants::{ExtensionSwap, PrefixVariants};
pub use words::{WordAdd, WordSubstitution};
