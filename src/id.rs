#![allow(dead_code)]

use std::fmt;
use std::marker::PhantomData;
use std::{collections::HashMap, hash::Hash};

use highway::{HighwayHash, HighwayHasher, Key};

pub const HASH_KEY: [u64; 4] = [0xdeadbeef, 0xcafebabe, 0x4242, 0x6969];

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub struct TypedId<T>(u64, PhantomData<T>);

impl<T> From<&T> for TypedId<T>
where
    T: Hash,
{
    fn from(value: &T) -> Self {
        let hash_key = Key(HASH_KEY);
        let mut hasher = HighwayHasher::new(hash_key);
        value.hash(&mut hasher);
        let id = hasher.finalize64();
        Self(id, PhantomData)
    }
}

impl<T> TypedId<T> {
    /// Returns the inner u64 value of the TypedId.
    pub fn value(&self) -> u64 {
        self.0
    }
}

impl<T> fmt::Display for TypedId<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Id({})", self.0)
    }
}

/// A registry to manage mappings between TypedIds and their corresponding values.
pub struct Registry<T: Hash + Eq + Clone> {
    mapping: HashMap<TypedId<T>, T>,
}

impl<T: Hash + Eq + Clone> Registry<T> {
    /// Creates a new, empty Registry.
    pub fn new() -> Self {
        Self {
            mapping: HashMap::new(),
        }
    }

    /// Registers a value and returns its corresponding TypedId.
    pub fn register(&mut self, value: T) -> TypedId<T> {
        let id = TypedId::<T>::from(&value);
        self.mapping.entry(id.clone()).or_insert(value);
        id
    }

    // Validate if the provided inner value corresponds to a valid mapping
    // and returns it as a type.
    pub fn is_valid(&self, id: u64) -> Option<TypedId<T>> {
        let typed_id = TypedId::<T>(id, PhantomData);
        self.mapping.contains_key(&typed_id).then_some(typed_id)
    }

    pub fn get(&self, id: &TypedId<T>) -> &T {
        // TypedId for a structure only exist if the structure is register. Only public
        // function which returns TypedId is is_valid and register both of which make sure
        // it returns only valid Id for a strucure which is already in the map. Thus we can
        // call unwrap here.
        self.mapping.get(id).unwrap()
    }

    pub fn ids(&self) -> impl Iterator<Item = &TypedId<T>> {
        self.mapping.keys()
    }
}

/// Macro to define a thread-safe, asynchronous registry for a given type.
///
/// This macro creates a new struct with methods to register, retrieve, and validate items.
/// The registry uses interior mutability with Arc and Mutex for thread-safe access.
///
/// # Arguments
/// * `$registry_name` - The name of the registry struct to be created
/// * `$item_type` - The type of items to be stored in the registry
///
/// # Example
/// ```rust
/// // Use the macro to define an ImageRegistry
/// define_registry!(ImageRegistry, ImageOption);
/// ```
///
#[macro_export]
macro_rules! define_registry {
    ($registry_name:ident, $item_type:ty) => {
        /// A thread-safe, asynchronous registry for storing and retrieving items.
        #[derive(Clone)]
        pub struct $registry_name {
            inner: std::sync::Arc<tokio::sync::Mutex<Registry<$item_type>>>,
        }

        impl $registry_name {
            /// Creates a new, empty registry.
            pub fn new() -> Self {
                Self {
                    inner: std::sync::Arc::new(tokio::sync::Mutex::new(
                        Registry::<$item_type>::new(),
                    )),
                }
            }

            /// Registers a new item in the registry and returns its TypedId.
            pub async fn register(&self, option: $item_type) -> TypedId<$item_type> {
                self.inner.lock().await.register(option)
            }

            /// Retrieves an item from the registry by its TypedId.
            pub async fn get(&self, id: &TypedId<$item_type>) -> $item_type {
                self.inner.lock().await.get(id).clone()
            }

            /// Checks if a given u64 value corresponds to a valid TypedId in the registry.
            pub async fn is_valid(&self, id: u64) -> Option<TypedId<$item_type>> {
                self.inner.lock().await.is_valid(id)
            }

            /// Returns a vector of all TypedIds currently in the registry.
            pub async fn ids(&self) -> Vec<TypedId<$item_type>> {
                self.inner.lock().await.ids().cloned().collect()
            }
        }
    };
}
