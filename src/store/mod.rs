//! Wrappers for working with terminus-store
//!
//! It is expected that most users of this library will work exclusively with the types contained in this module.
pub mod sync;

use futures::prelude::*;
use futures::future;
use std::sync::Arc;
use std::path::PathBuf;

use futures_locks::RwLock;

use crate::storage::{LabelStore, LayerStore, MemoryLabelStore, MemoryLayerStore, DirectoryLabelStore, DirectoryLayerStore, CachedLayerStore};
use crate::layer::{Layer,LayerBuilder,ObjectType,StringTriple,IdTriple,SubjectLookup,ObjectLookup};

use std::io;

/// A wrapper over a SimpleLayerBuilder, providing a thread-safe sharable interface
///
/// The SimpleLayerBuilder requires one to have a mutable reference to
/// it, and on commit it will be consumed. This builder only requires
/// an immutable reference, and uses a futures-aware read-write lock
/// to synchronize access to it between threads. Also, rather than
/// consuming itself on commit, this wrapper will simply mark itself
/// as having committed, returning errors on further calls.
pub struct DatabaseLayerBuilder {
    builder: RwLock<Option<Box<dyn LayerBuilder>>>,
    name: [u32;5],
    store: Store
}

impl DatabaseLayerBuilder {
    fn new(store: Store) -> impl Future<Item=Self,Error=io::Error>+Send+Sync {
        store.layer_store.create_base_layer()
            .map(|builder|
                 Self {
                     name: builder.name(),
                     builder: RwLock::new(Some(builder)),
                     store 
                 })
    }

    fn wrap(builder: Box<dyn LayerBuilder>, store: Store) -> Self {
        DatabaseLayerBuilder {
            name: builder.name(),
            builder: RwLock::new(Some(builder)),
            store
        }
    }

    fn with_builder<R:Send+Sync,F: FnOnce(&mut Box<dyn LayerBuilder>)->R+Send+Sync>(&self, f: F) -> impl Future<Item=R,Error=io::Error>+Send+Sync {
        self.builder.write()
            .then(|b| {
                let mut builder = b.expect("rwlock write should always succeed");
                match (*builder).as_mut() {
                    None => future::err(io::Error::new(io::ErrorKind::InvalidData, "builder has already been committed")),
                    Some(builder) => future::ok(f(builder))
                }
            })
    }

    /// Returns the name of the layer being built
    pub fn name(&self) -> [u32;5] {
        self.name
    }

    /// Add a string triple
    pub fn add_string_triple(&self, triple: &StringTriple) -> impl Future<Item=(),Error=io::Error>+Send+Sync {
        let triple = triple.clone();
        self.with_builder(move |b|b.add_string_triple(&triple))
    }

    /// Add an id triple
    pub fn add_id_triple(&self, triple: IdTriple) -> impl Future<Item=bool,Error=io::Error>+Send+Sync {
        self.with_builder(move |b|b.add_id_triple(triple))
    }

    /// Remove a string triple
    pub fn remove_string_triple(&self, triple: &StringTriple) -> impl Future<Item=bool,Error=io::Error>+Send+Sync {
        let triple = triple.clone();
        self.with_builder(move |b|b.remove_string_triple(&triple))
    }

    /// Remove an id triple
    pub fn remove_id_triple(&self, triple: IdTriple) -> impl Future<Item=bool,Error=io::Error>+Send+Sync {
        self.with_builder(move |b|b.remove_id_triple(triple))
    }

    /// Commit the layer to storage
    pub fn commit(&self) -> impl Future<Item=DatabaseLayer, Error=std::io::Error>+Send+Sync {
        let store = self.store.clone();
        let name = self.name;
        self.builder.write()
            .then(move |b| {
                let mut swap = b.expect("rwlock write should always succeed");
                let mut builder = None;

                std::mem::swap(&mut builder, &mut swap);

                let result: Box<dyn Future<Item=_,Error=_>+Send+Sync> =
                    match builder {
                        None => Box::new(future::err(io::Error::new(io::ErrorKind::InvalidData, "builder has already been committed"))),
                        Some(builder) => Box::new( 
                            builder.commit_boxed()
                                .and_then(move |_| store.layer_store.get_layer(name)
                                          .map(move |layer| DatabaseLayer::wrap(layer.expect("layer that was just created was not found in store"), store))))
                    };

                result
            })
    }
}

/// A layer that keeps track of the store it came out of, allowing the creation of a layer builder on top of this layer
pub struct DatabaseLayer {
    layer: Arc<dyn Layer>,
    store: Store
}

impl DatabaseLayer {
    fn wrap(layer: Arc<dyn Layer>, store: Store) -> Self {
        DatabaseLayer {
            layer, store
        }
    }

    /// Create a layer builder based on this layer
    pub fn open_write(&self) -> impl Future<Item=DatabaseLayerBuilder,Error=io::Error>+Send+Sync {
        let store = self.store.clone();
        self.store.layer_store.create_child_layer(self.layer.name())
            .map(move |layer|DatabaseLayerBuilder::wrap(layer, store))
    }
}

impl Layer for DatabaseLayer {
    fn name(&self) -> [u32;5] {
        self.layer.name()
    }

    fn parent(&self) -> Option<Arc<dyn Layer>> {
        self.layer.parent()
    }

    fn node_and_value_count(&self) -> usize {
        self.layer.node_and_value_count()
    }

    fn predicate_count(&self) -> usize {
        self.layer.predicate_count()
    }

    fn subject_id(&self, subject: &str) -> Option<u64> {
        self.layer.subject_id(subject)
    }

    fn predicate_id(&self, predicate: &str) -> Option<u64> {
        self.layer.predicate_id(predicate)
    }

    fn object_node_id(&self, object: &str) -> Option<u64> {
        self.layer.object_node_id(object)
    }

    fn object_value_id(&self, object: &str) -> Option<u64> {
        self.layer.object_value_id(object)
    }

    fn id_subject(&self, id: u64) -> Option<String> {
        self.layer.id_subject(id)
    }

    fn id_predicate(&self, id: u64) -> Option<String> {
        self.layer.id_predicate(id)
    }

    fn id_object(&self, id: u64) -> Option<ObjectType> {
        self.layer.id_object(id)
    }

    fn subjects(&self) -> Box<dyn Iterator<Item=Box<dyn SubjectLookup>>> {
        self.layer.subjects()
    }

    fn lookup_subject(&self, subject: u64) -> Option<Box<dyn SubjectLookup>> {
        self.layer.lookup_subject(subject)
    }

    fn objects(&self) -> Box<dyn Iterator<Item=Box<dyn ObjectLookup>>> {
        self.layer.objects()
    }

    fn lookup_object(&self, object: u64) -> Option<Box<dyn ObjectLookup>> {
        self.layer.lookup_object(object)
    }
}


/// A database in terminus-store
///
/// Databases in terminus-store are basically just a label pointing to
/// a layer. Opening a read transaction to a database is just getting
/// hold of the layer it points at, as layers are read-only. Writing
/// to a database is just making it point to a new layer.
pub struct Database {
    label: String,
    store: Store
}

impl Database {
    fn new(label: String, store: Store) -> Self {
        Database {
            label,
            store
        }
    }

    /// Returns the layer this database points at
    pub fn head(&self) -> impl Future<Item=Option<DatabaseLayer>,Error=io::Error>+Send+Sync {
        let store = self.store.clone();
        store.label_store.get_label(&self.label)
            .and_then(move |new_label| {
                match new_label {
                    None => Box::new(future::err(io::Error::new(io::ErrorKind::NotFound, "database not found"))),
                    Some(new_label) => {
                        let result: Box<dyn Future<Item=_,Error=_>+Send+Sync> =
                            match new_label.layer {
                                None => Box::new(future::ok(None)),
                                Some(layer) => Box::new(store.layer_store.get_layer(layer)
                                                        .map(move |layer| layer.map(move |layer|DatabaseLayer::wrap(layer, store))))
                            };
                        result
                    }
                }
            })
    }
    
    /// Set the database label to the given layer if it is a valid ancestor, returning false otherwise
    pub fn set_head(&self, layer: &DatabaseLayer) -> impl Future<Item=bool,Error=io::Error>+Send+Sync {
        let store = self.store.clone();
        let layer_name = layer.name();
        let cloned_layer = layer.layer.clone();
        store.label_store.get_label(&self.label)
            .and_then(move |label| {
                let result: Box<dyn Future<Item=_,Error=_>+Send+Sync> = 
                    match label {
                        None => Box::new(future::err(io::Error::new(io::ErrorKind::NotFound, "label not found"))),
                        Some(label) => Box::new({
                            let result: Box<dyn Future<Item=_,Error=_>+Send+Sync> =
                                match label.layer {
                                    None => Box::new(future::ok(true)),
                                    Some(layer_name) => Box::new(store.layer_store.get_layer(layer_name)
                                                                 .map(move |l|l.map(|l|l.is_ancestor_of(&*cloned_layer)).unwrap_or(false)))
                                };

                            result
                        }.and_then(move |b| {
                            let result: Box<dyn Future<Item=_,Error=_>+Send+Sync> =
                                if b {
                                    Box::new(store.label_store.set_label(&label, layer_name).map(|_|true))
                                } else {
                                    Box::new(future::ok(false))
                                };

                            result
                        }))
                    };
                result
            })

    }
}

/// A store, storing a set of layers and database labels pointing to these layers
#[derive(Clone)]
pub struct Store {
    label_store: Arc<dyn LabelStore>,
    layer_store: Arc<dyn LayerStore>,
}

impl Store {
    /// Create a new store from the given label and layer store
    pub fn new<Labels:'static+LabelStore, Layers:'static+LayerStore>(label_store: Labels, layer_store: Layers) -> Store {
        Store {
            label_store: Arc::new(label_store),
            layer_store: Arc::new(layer_store),
        }
    }

    /// Create a new database with the given name
    ///
    /// If the database already exists, this will return an error
    pub fn create(&self, label: &str) -> impl Future<Item=Database,Error=std::io::Error>+Send+Sync {
        let store = self.clone();
        self.label_store.create_label(label)
            .map(move |label| Database::new(label.name, store))
    }

    /// Open an existing database with the given name, or None if it does not exist
    pub fn open(&self, label: &str) -> impl Future<Item=Option<Database>,Error=std::io::Error> {
        let store = self.clone();
        self.label_store.get_label(label)
            .map(move |label| label.map(|label|Database::new(label.name, store)))
    }

    /// Create a base layer builder, unattached to any database label
    ///
    /// After having committed it, use `set_head` on a `Database` to attach it.
    pub fn create_base_layer(&self) -> impl Future<Item=DatabaseLayerBuilder,Error=io::Error>+Send+Sync {
        DatabaseLayerBuilder::new(self.clone())
    }
}

/// Open a store that is entirely in memory
///
/// This is useful for testing purposes, or if the database is only going to be used for caching purposes
pub fn open_memory_store() -> Store {
    Store::new(MemoryLabelStore::new(), CachedLayerStore::new(MemoryLayerStore::new()))
}

/// Open a store that stores its data in the given directory
pub fn open_directory_store<P:Into<PathBuf>>(path: P) -> Store {
    let p = path.into();
    Store::new(DirectoryLabelStore::new(p.clone()), CachedLayerStore::new(DirectoryLayerStore::new(p)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::runtime::Runtime;
    use futures::sync::oneshot;
    use tempfile::tempdir;

    #[test]
    fn create_and_manipulate_memory_database() {
        let runtime = Runtime::new().unwrap();

        let store = open_memory_store();
        let database = oneshot::spawn(store.create("foodb"), &runtime.executor()).wait().unwrap();

        let head = oneshot::spawn(database.head(), &runtime.executor()).wait().unwrap();
        assert!(head.is_none());

        let mut builder = oneshot::spawn(store.create_base_layer(), &runtime.executor()).wait().unwrap();
        oneshot::spawn(builder.add_string_triple(&StringTriple::new_value("cow","says","moo")), &runtime.executor()).wait().unwrap();

        let layer = oneshot::spawn(builder.commit(), &runtime.executor()).wait().unwrap();
        assert!(oneshot::spawn(database.set_head(&layer), &runtime.executor()).wait().unwrap());

        builder = oneshot::spawn(layer.open_write(), &runtime.executor()).wait().unwrap();
        oneshot::spawn(builder.add_string_triple(&StringTriple::new_value("pig","says","oink")), &runtime.executor()).wait().unwrap();

        let layer2 = oneshot::spawn(builder.commit(), &runtime.executor()).wait().unwrap();
        assert!(oneshot::spawn(database.set_head(&layer2), &runtime.executor()).wait().unwrap());
        let layer2_name = layer2.name();

        let layer = oneshot::spawn(database.head(), &runtime.executor()).wait().unwrap().unwrap();

        assert_eq!(layer2_name, layer.name());
        assert!(layer.string_triple_exists(&StringTriple::new_value("cow","says","moo")));
        assert!(layer.string_triple_exists(&StringTriple::new_value("pig","says","oink")));
    }

    #[test]
    fn create_and_manipulate_directory_database() {
        let runtime = Runtime::new().unwrap();
        let dir = tempdir().unwrap();

        let store = open_directory_store(dir.path());
        let database = oneshot::spawn(store.create("foodb"), &runtime.executor()).wait().unwrap();

        let head = oneshot::spawn(database.head(), &runtime.executor()).wait().unwrap();
        assert!(head.is_none());

        let mut builder = oneshot::spawn(store.create_base_layer(), &runtime.executor()).wait().unwrap();
        oneshot::spawn(builder.add_string_triple(&StringTriple::new_value("cow","says","moo")), &runtime.executor()).wait().unwrap();

        let layer = oneshot::spawn(builder.commit(), &runtime.executor()).wait().unwrap();
        assert!(oneshot::spawn(database.set_head(&layer), &runtime.executor()).wait().unwrap());

        builder = oneshot::spawn(layer.open_write(), &runtime.executor()).wait().unwrap();
        oneshot::spawn(builder.add_string_triple(&StringTriple::new_value("pig","says","oink")), &runtime.executor()).wait().unwrap();

        let layer2 = oneshot::spawn(builder.commit(), &runtime.executor()).wait().unwrap();
        assert!(oneshot::spawn(database.set_head(&layer2), &runtime.executor()).wait().unwrap());
        let layer2_name = layer2.name();

        let layer = oneshot::spawn(database.head(), &runtime.executor()).wait().unwrap().unwrap();

        assert_eq!(layer2_name, layer.name());
        assert!(layer.string_triple_exists(&StringTriple::new_value("cow","says","moo")));
        assert!(layer.string_triple_exists(&StringTriple::new_value("pig","says","oink")));
    }
}
