//! Validates all used crates and extern libraries and loads their metadata

use crate::cstore::{self, CStore, CrateSource, MetadataBlob};
use crate::locator::{self, CratePaths};
use crate::schema::{CrateRoot, CrateDep};
use rustc_data_structures::sync::{Lrc, RwLock, Lock, AtomicCell};

use rustc::hir::def_id::CrateNum;
use rustc_data_structures::svh::Svh;
use rustc::dep_graph::DepNodeIndex;
use rustc::middle::cstore::DepKind;
use rustc::mir::interpret::AllocDecodingState;
use rustc::session::{Session, CrateDisambiguator};
use rustc::session::config::{Sanitizer, self};
use rustc_target::spec::{PanicStrategy, TargetTriple};
use rustc::session::search_paths::PathKind;
use rustc::middle::cstore::{ExternCrate, ExternCrateSource};
use rustc::util::common::record_time;
use rustc::util::nodemap::FxHashSet;
use rustc::hir::map::Definitions;
use rustc::hir::def_id::LOCAL_CRATE;

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::{cmp, fs};

use syntax::ast;
use syntax::attr;
use syntax::ext::allocator::{global_allocator_spans, AllocatorKind};
use syntax::symbol::{Symbol, sym};
use syntax::{span_err, span_fatal};
use syntax_pos::{Span, DUMMY_SP};
use log::{debug, info, log_enabled};
use proc_macro::bridge::client::ProcMacro;

pub struct Library {
    pub dylib: Option<(PathBuf, PathKind)>,
    pub rlib: Option<(PathBuf, PathKind)>,
    pub rmeta: Option<(PathBuf, PathKind)>,
    pub metadata: MetadataBlob,
}

pub struct CrateLoader<'a> {
    pub sess: &'a Session,
    cstore: &'a CStore,
    local_crate_name: Symbol,
}

fn dump_crates(cstore: &CStore) {
    info!("resolved crates:");
    cstore.iter_crate_data(|_, data| {
        info!("  name: {}", data.root.name);
        info!("  cnum: {}", data.cnum);
        info!("  hash: {}", data.root.hash);
        info!("  reqd: {:?}", *data.dep_kind.lock());
        let CrateSource { dylib, rlib, rmeta } = data.source.clone();
        dylib.map(|dl| info!("  dylib: {}", dl.0.display()));
        rlib.map(|rl|  info!("   rlib: {}", rl.0.display()));
        rmeta.map(|rl| info!("   rmeta: {}", rl.0.display()));
    });
}

// Extra info about a crate loaded for plugins or exported macros.
struct ExtensionCrate {
    metadata: PMDSource,
    dylib: Option<PathBuf>,
    target_only: bool,
}

enum PMDSource {
    Registered(Lrc<cstore::CrateMetadata>),
    Owned(Library),
}

impl Deref for PMDSource {
    type Target = MetadataBlob;

    fn deref(&self) -> &MetadataBlob {
        match *self {
            PMDSource::Registered(ref cmd) => &cmd.blob,
            PMDSource::Owned(ref lib) => &lib.metadata
        }
    }
}

enum LoadResult {
    Previous(CrateNum),
    Loaded(Library),
}

enum LoadError<'a> {
    LocatorError(locator::Context<'a>),
}

impl<'a> LoadError<'a> {
    fn report(self) -> ! {
        match self {
            LoadError::LocatorError(locate_ctxt) => locate_ctxt.report_errs(),
        }
    }
}

impl<'a> CrateLoader<'a> {
    pub fn new(sess: &'a Session, cstore: &'a CStore, local_crate_name: &str) -> Self {
        CrateLoader {
            sess,
            cstore,
            local_crate_name: Symbol::intern(local_crate_name),
        }
    }

    fn existing_match(&self, name: Symbol, hash: Option<&Svh>, kind: PathKind)
                      -> Option<CrateNum> {
        let mut ret = None;
        self.cstore.iter_crate_data(|cnum, data| {
            if data.root.name != name { return }

            match hash {
                Some(hash) if *hash == data.root.hash => { ret = Some(cnum); return }
                Some(..) => return,
                None => {}
            }

            // When the hash is None we're dealing with a top-level dependency
            // in which case we may have a specification on the command line for
            // this library. Even though an upstream library may have loaded
            // something of the same name, we have to make sure it was loaded
            // from the exact same location as well.
            //
            // We're also sure to compare *paths*, not actual byte slices. The
            // `source` stores paths which are normalized which may be different
            // from the strings on the command line.
            let source = &self.cstore.get_crate_data(cnum).source;
            if let Some(entry) = self.sess.opts.externs.get(&*name.as_str()) {
                // Only use `--extern crate_name=path` here, not `--extern crate_name`.
                let found = entry.locations.iter().filter_map(|l| l.as_ref()).any(|l| {
                    let l = fs::canonicalize(l).ok();
                    source.dylib.as_ref().map(|p| &p.0) == l.as_ref() ||
                    source.rlib.as_ref().map(|p| &p.0) == l.as_ref()
                });
                if found {
                    ret = Some(cnum);
                }
                return
            }

            // Alright, so we've gotten this far which means that `data` has the
            // right name, we don't have a hash, and we don't have a --extern
            // pointing for ourselves. We're still not quite yet done because we
            // have to make sure that this crate was found in the crate lookup
            // path (this is a top-level dependency) as we don't want to
            // implicitly load anything inside the dependency lookup path.
            let prev_kind = source.dylib.as_ref().or(source.rlib.as_ref())
                                  .or(source.rmeta.as_ref())
                                  .expect("No sources for crate").1;
            if ret.is_none() && (prev_kind == kind || prev_kind == PathKind::All) {
                ret = Some(cnum);
            }
        });
        return ret;
    }

    fn verify_no_symbol_conflicts(&self,
                                  span: Span,
                                  root: &CrateRoot<'_>) {
        // Check for (potential) conflicts with the local crate
        if self.local_crate_name == root.name &&
           self.sess.local_crate_disambiguator() == root.disambiguator {
            span_fatal!(self.sess, span, E0519,
                        "the current crate is indistinguishable from one of its \
                         dependencies: it has the same crate-name `{}` and was \
                         compiled with the same `-C metadata` arguments. This \
                         will result in symbol conflicts between the two.",
                        root.name)
        }

        // Check for conflicts with any crate loaded so far
        self.cstore.iter_crate_data(|_, other| {
            if other.root.name == root.name && // same crate-name
               other.root.disambiguator == root.disambiguator &&  // same crate-disambiguator
               other.root.hash != root.hash { // but different SVH
                span_fatal!(self.sess, span, E0523,
                        "found two different crates with name `{}` that are \
                         not distinguished by differing `-C metadata`. This \
                         will result in symbol conflicts between the two.",
                        root.name)
            }
        });
    }

    fn register_crate(
        &mut self,
        host_lib: Option<Library>,
        root: Option<&CratePaths>,
        span: Span,
        lib: Library,
        dep_kind: DepKind,
        name: Symbol
    ) -> (CrateNum, Lrc<cstore::CrateMetadata>) {
        let _prof_timer =
            self.sess.prof.generic_activity("metadata_register_crate");

        let crate_root = lib.metadata.get_root();
        self.verify_no_symbol_conflicts(span, &crate_root);

        let private_dep = self.sess.opts.externs.get(&name.as_str())
            .map(|e| e.is_private_dep)
            .unwrap_or(false);

        info!("register crate `{}` (private_dep = {})", crate_root.name, private_dep);

        // Claim this crate number and cache it
        let cnum = self.cstore.alloc_new_crate_num();

        // Maintain a reference to the top most crate.
        // Stash paths for top-most crate locally if necessary.
        let crate_paths;
        let root = if let Some(root) = root {
            root
        } else {
            crate_paths = CratePaths {
                ident: crate_root.name.to_string(),
                dylib: lib.dylib.clone().map(|p| p.0),
                rlib:  lib.rlib.clone().map(|p| p.0),
                rmeta: lib.rmeta.clone().map(|p| p.0),
            };
            &crate_paths
        };

        let Library { dylib, rlib, rmeta, metadata } = lib;
        let cnum_map = self.resolve_crate_deps(root, &crate_root, &metadata, cnum, span, dep_kind);

        let dependencies: Vec<CrateNum> = cnum_map.iter().cloned().collect();

        let raw_proc_macros =  crate_root.proc_macro_data.map(|_| {
            let temp_root;
            let (dlsym_dylib, dlsym_root) = match &host_lib {
                Some(host_lib) =>
                    (&host_lib.dylib, { temp_root = host_lib.metadata.get_root(); &temp_root }),
                None => (&dylib, &crate_root),
            };
            let dlsym_dylib = dlsym_dylib.as_ref().expect("no dylib for a proc-macro crate");
            self.dlsym_proc_macros(&dlsym_dylib.0, dlsym_root.disambiguator, span)
        });

        let interpret_alloc_index: Vec<u32> = crate_root.interpret_alloc_index
                                                        .decode(&metadata)
                                                        .collect();
        let trait_impls = crate_root
            .impls
            .decode((&metadata, self.sess))
            .map(|trait_impls| (trait_impls.trait_id, trait_impls.impls))
            .collect();

        let def_path_table = record_time(&self.sess.perf_stats.decode_def_path_tables_time, || {
            crate_root.def_path_table.decode((&metadata, self.sess))
        });

        let cmeta = cstore::CrateMetadata {
            extern_crate: Lock::new(None),
            def_path_table: Lrc::new(def_path_table),
            trait_impls,
            root: crate_root,
            blob: metadata,
            cnum_map,
            cnum,
            dependencies: Lock::new(dependencies),
            source_map_import_info: RwLock::new(vec![]),
            alloc_decoding_state: AllocDecodingState::new(interpret_alloc_index),
            dep_kind: Lock::new(dep_kind),
            source: cstore::CrateSource {
                dylib,
                rlib,
                rmeta,
            },
            private_dep,
            span,
            raw_proc_macros,
            dep_node_index: AtomicCell::new(DepNodeIndex::INVALID),
        };

        let cmeta = Lrc::new(cmeta);
        self.cstore.set_crate_data(cnum, cmeta.clone());
        (cnum, cmeta)
    }

    fn load_proc_macro<'b>(
        &mut self,
        locate_ctxt: &mut locator::Context<'b>,
        path_kind: PathKind,
    ) -> Option<(LoadResult, Option<Library>)>
    where
        'a: 'b,
    {
        // Use a new locator Context so trying to load a proc macro doesn't affect the error
        // message we emit
        let mut proc_macro_locator = locate_ctxt.clone();

        // Try to load a proc macro
        proc_macro_locator.is_proc_macro = Some(true);

        // Load the proc macro crate for the target
        let (locator, target_result) = if self.sess.opts.debugging_opts.dual_proc_macros {
            proc_macro_locator.reset();
            let result = match self.load(&mut proc_macro_locator)? {
                LoadResult::Previous(cnum) => return Some((LoadResult::Previous(cnum), None)),
                LoadResult::Loaded(library) => Some(LoadResult::Loaded(library))
            };
            // Don't look for a matching hash when looking for the host crate.
            // It won't be the same as the target crate hash
            locate_ctxt.hash = None;
            // Use the locate_ctxt when looking for the host proc macro crate, as that is required
            // so we want it to affect the error message
            (locate_ctxt, result)
        } else {
            (&mut proc_macro_locator, None)
        };

        // Load the proc macro crate for the host

        locator.reset();
        locator.is_proc_macro = Some(true);
        locator.target = &self.sess.host;
        locator.triple = TargetTriple::from_triple(config::host_triple());
        locator.filesearch = self.sess.host_filesearch(path_kind);

        let host_result = self.load(locator)?;

        Some(if self.sess.opts.debugging_opts.dual_proc_macros {
            let host_result = match host_result {
                LoadResult::Previous(..) => {
                    panic!("host and target proc macros must be loaded in lock-step")
                }
                LoadResult::Loaded(library) => library
            };
            (target_result.unwrap(), Some(host_result))
        } else {
            (host_result, None)
        })
    }

    fn resolve_crate<'b>(
        &'b mut self,
        name: Symbol,
        span: Span,
        dep_kind: DepKind,
        dep: Option<(&'b CratePaths, &'b CrateDep)>,
    ) -> (CrateNum, Lrc<cstore::CrateMetadata>) {
        self.maybe_resolve_crate(name, span, dep_kind, dep).unwrap_or_else(|err| err.report())
    }

    fn maybe_resolve_crate<'b>(
        &'b mut self,
        name: Symbol,
        span: Span,
        mut dep_kind: DepKind,
        dep: Option<(&'b CratePaths, &'b CrateDep)>,
    ) -> Result<(CrateNum, Lrc<cstore::CrateMetadata>), LoadError<'b>> {
        info!("resolving crate `{}`", name);
        let (root, hash, extra_filename, path_kind) = match dep {
            Some((root, dep)) =>
                (Some(root), Some(&dep.hash), Some(&dep.extra_filename[..]), PathKind::Dependency),
            None => (None, None, None, PathKind::Crate),
        };
        let result = if let Some(cnum) = self.existing_match(name, hash, path_kind) {
            (LoadResult::Previous(cnum), None)
        } else {
            info!("falling back to a load");
            let mut locate_ctxt = locator::Context {
                sess: self.sess,
                span,
                crate_name: name,
                hash,
                extra_filename,
                filesearch: self.sess.target_filesearch(path_kind),
                target: &self.sess.target.target,
                triple: self.sess.opts.target_triple.clone(),
                root,
                rejected_via_hash: vec![],
                rejected_via_triple: vec![],
                rejected_via_kind: vec![],
                rejected_via_version: vec![],
                rejected_via_filename: vec![],
                should_match_name: true,
                is_proc_macro: Some(false),
                metadata_loader: &*self.cstore.metadata_loader,
            };

            self.load(&mut locate_ctxt).map(|r| (r, None)).or_else(|| {
                dep_kind = DepKind::UnexportedMacrosOnly;
                self.load_proc_macro(&mut locate_ctxt, path_kind)
            }).ok_or_else(move || LoadError::LocatorError(locate_ctxt))?
        };

        match result {
            (LoadResult::Previous(cnum), None) => {
                let data = self.cstore.get_crate_data(cnum);
                if data.root.proc_macro_data.is_some() {
                    dep_kind = DepKind::UnexportedMacrosOnly;
                }
                data.dep_kind.with_lock(|data_dep_kind| {
                    *data_dep_kind = cmp::max(*data_dep_kind, dep_kind);
                });
                Ok((cnum, data))
            }
            (LoadResult::Loaded(library), host_library) => {
                Ok(self.register_crate(host_library, root, span, library, dep_kind, name))
            }
            _ => panic!()
        }
    }

    fn load(&mut self, locate_ctxt: &mut locator::Context<'_>) -> Option<LoadResult> {
        let library = locate_ctxt.maybe_load_library_crate()?;

        // In the case that we're loading a crate, but not matching
        // against a hash, we could load a crate which has the same hash
        // as an already loaded crate. If this is the case prevent
        // duplicates by just using the first crate.
        //
        // Note that we only do this for target triple crates, though, as we
        // don't want to match a host crate against an equivalent target one
        // already loaded.
        let root = library.metadata.get_root();
        if locate_ctxt.triple == self.sess.opts.target_triple {
            let mut result = LoadResult::Loaded(library);
            self.cstore.iter_crate_data(|cnum, data| {
                if data.root.name == root.name && root.hash == data.root.hash {
                    assert!(locate_ctxt.hash.is_none());
                    info!("load success, going to previous cnum: {}", cnum);
                    result = LoadResult::Previous(cnum);
                }
            });
            Some(result)
        } else {
            Some(LoadResult::Loaded(library))
        }
    }

    fn update_extern_crate(&mut self,
                           cnum: CrateNum,
                           mut extern_crate: ExternCrate,
                           visited: &mut FxHashSet<(CrateNum, bool)>)
    {
        if !visited.insert((cnum, extern_crate.is_direct())) { return }

        let cmeta = self.cstore.get_crate_data(cnum);
        let mut old_extern_crate = cmeta.extern_crate.borrow_mut();

        // Prefer:
        // - something over nothing (tuple.0);
        // - direct extern crate to indirect (tuple.1);
        // - shorter paths to longer (tuple.2).
        let new_rank = (
            true,
            extern_crate.is_direct(),
            cmp::Reverse(extern_crate.path_len),
        );
        let old_rank = match *old_extern_crate {
            None => (false, false, cmp::Reverse(usize::max_value())),
            Some(ref c) => (
                true,
                c.is_direct(),
                cmp::Reverse(c.path_len),
            ),
        };
        if old_rank >= new_rank {
            return; // no change needed
        }

        *old_extern_crate = Some(extern_crate);
        drop(old_extern_crate);

        // Propagate the extern crate info to dependencies.
        extern_crate.dependency_of = cnum;
        for &dep_cnum in cmeta.dependencies.borrow().iter() {
            self.update_extern_crate(dep_cnum, extern_crate, visited);
        }
    }

    // Go through the crate metadata and load any crates that it references
    fn resolve_crate_deps(&mut self,
                          root: &CratePaths,
                          crate_root: &CrateRoot<'_>,
                          metadata: &MetadataBlob,
                          krate: CrateNum,
                          span: Span,
                          dep_kind: DepKind)
                          -> cstore::CrateNumMap {
        debug!("resolving deps of external crate");
        if crate_root.proc_macro_data.is_some() {
            return cstore::CrateNumMap::new();
        }

        // The map from crate numbers in the crate we're resolving to local crate numbers.
        // We map 0 and all other holes in the map to our parent crate. The "additional"
        // self-dependencies should be harmless.
        std::iter::once(krate).chain(crate_root.crate_deps.decode(metadata).map(|dep| {
            info!("resolving dep crate {} hash: `{}` extra filename: `{}`", dep.name, dep.hash,
                  dep.extra_filename);
            if dep.kind == DepKind::UnexportedMacrosOnly {
                return krate;
            }
            let dep_kind = match dep_kind {
                DepKind::MacrosOnly => DepKind::MacrosOnly,
                _ => dep.kind,
            };
            self.resolve_crate(dep.name, span, dep_kind, Some((root, &dep))).0
        })).collect()
    }

    fn read_extension_crate(&mut self, name: Symbol, span: Span) -> ExtensionCrate {
        info!("read extension crate `{}`", name);
        let target_triple = self.sess.opts.target_triple.clone();
        let host_triple = TargetTriple::from_triple(config::host_triple());
        let is_cross = target_triple != host_triple;
        let mut target_only = false;
        let mut locate_ctxt = locator::Context {
            sess: self.sess,
            span,
            crate_name: name,
            hash: None,
            extra_filename: None,
            filesearch: self.sess.host_filesearch(PathKind::Crate),
            target: &self.sess.host,
            triple: host_triple,
            root: None,
            rejected_via_hash: vec![],
            rejected_via_triple: vec![],
            rejected_via_kind: vec![],
            rejected_via_version: vec![],
            rejected_via_filename: vec![],
            should_match_name: true,
            is_proc_macro: None,
            metadata_loader: &*self.cstore.metadata_loader,
        };
        let library = self.load(&mut locate_ctxt).or_else(|| {
            if !is_cross {
                return None
            }
            // Try loading from target crates. This will abort later if we
            // try to load a plugin registrar function,
            target_only = true;

            locate_ctxt.target = &self.sess.target.target;
            locate_ctxt.triple = target_triple;
            locate_ctxt.filesearch = self.sess.target_filesearch(PathKind::Crate);

            self.load(&mut locate_ctxt)
        });
        let library = match library {
            Some(l) => l,
            None => locate_ctxt.report_errs(),
        };

        let (dylib, metadata) = match library {
            LoadResult::Previous(cnum) => {
                let data = self.cstore.get_crate_data(cnum);
                (data.source.dylib.clone(), PMDSource::Registered(data))
            }
            LoadResult::Loaded(library) => {
                let dylib = library.dylib.clone();
                let metadata = PMDSource::Owned(library);
                (dylib, metadata)
            }
        };

        ExtensionCrate {
            metadata,
            dylib: dylib.map(|p| p.0),
            target_only,
        }
    }

    fn dlsym_proc_macros(&self,
                         path: &Path,
                         disambiguator: CrateDisambiguator,
                         span: Span
    ) -> &'static [ProcMacro] {
        use std::env;
        use crate::dynamic_lib::DynamicLibrary;

        // Make sure the path contains a / or the linker will search for it.
        let path = env::current_dir().unwrap().join(path);
        let lib = match DynamicLibrary::open(Some(&path)) {
            Ok(lib) => lib,
            Err(err) => self.sess.span_fatal(span, &err),
        };

        let sym = self.sess.generate_proc_macro_decls_symbol(disambiguator);
        let decls = unsafe {
            let sym = match lib.symbol(&sym) {
                Ok(f) => f,
                Err(err) => self.sess.span_fatal(span, &err),
            };
            *(sym as *const &[ProcMacro])
        };

        // Intentionally leak the dynamic library. We can't ever unload it
        // since the library can make things that will live arbitrarily long.
        std::mem::forget(lib);

        decls
    }

    /// Look for a plugin registrar. Returns library path, crate
    /// SVH and DefIndex of the registrar function.
    pub fn find_plugin_registrar(&mut self,
                                 span: Span,
                                 name: Symbol)
                                 -> Option<(PathBuf, CrateDisambiguator)> {
        let ekrate = self.read_extension_crate(name, span);

        if ekrate.target_only {
            // Need to abort before syntax expansion.
            let message = format!("plugin `{}` is not available for triple `{}` \
                                   (only found {})",
                                  name,
                                  config::host_triple(),
                                  self.sess.opts.target_triple);
            span_fatal!(self.sess, span, E0456, "{}", &message);
        }

        let root = ekrate.metadata.get_root();
        match ekrate.dylib.as_ref() {
            Some(dylib) => {
                Some((dylib.to_path_buf(), root.disambiguator))
            }
            None => {
                span_err!(self.sess, span, E0457,
                          "plugin `{}` only found in rlib format, but must be available \
                           in dylib format",
                          name);
                // No need to abort because the loading code will just ignore this
                // empty dylib.
                None
            }
        }
    }

    fn inject_panic_runtime(&mut self, krate: &ast::Crate) {
        // If we're only compiling an rlib, then there's no need to select a
        // panic runtime, so we just skip this section entirely.
        let any_non_rlib = self.sess.crate_types.borrow().iter().any(|ct| {
            *ct != config::CrateType::Rlib
        });
        if !any_non_rlib {
            info!("panic runtime injection skipped, only generating rlib");
            self.sess.injected_panic_runtime.set(None);
            return
        }

        // If we need a panic runtime, we try to find an existing one here. At
        // the same time we perform some general validation of the DAG we've got
        // going such as ensuring everything has a compatible panic strategy.
        //
        // The logic for finding the panic runtime here is pretty much the same
        // as the allocator case with the only addition that the panic strategy
        // compilation mode also comes into play.
        let desired_strategy = self.sess.panic_strategy();
        let mut runtime_found = false;
        let mut needs_panic_runtime = attr::contains_name(&krate.attrs,
                                                          sym::needs_panic_runtime);

        self.cstore.iter_crate_data(|cnum, data| {
            needs_panic_runtime = needs_panic_runtime ||
                                  data.root.needs_panic_runtime;
            if data.root.panic_runtime {
                // Inject a dependency from all #![needs_panic_runtime] to this
                // #![panic_runtime] crate.
                self.inject_dependency_if(cnum, "a panic runtime",
                                          &|data| data.root.needs_panic_runtime);
                runtime_found = runtime_found || *data.dep_kind.lock() == DepKind::Explicit;
            }
        });

        // If an explicitly linked and matching panic runtime was found, or if
        // we just don't need one at all, then we're done here and there's
        // nothing else to do.
        if !needs_panic_runtime || runtime_found {
            self.sess.injected_panic_runtime.set(None);
            return
        }

        // By this point we know that we (a) need a panic runtime and (b) no
        // panic runtime was explicitly linked. Here we just load an appropriate
        // default runtime for our panic strategy and then inject the
        // dependencies.
        //
        // We may resolve to an already loaded crate (as the crate may not have
        // been explicitly linked prior to this) and we may re-inject
        // dependencies again, but both of those situations are fine.
        //
        // Also note that we have yet to perform validation of the crate graph
        // in terms of everyone has a compatible panic runtime format, that's
        // performed later as part of the `dependency_format` module.
        let name = match desired_strategy {
            PanicStrategy::Unwind => Symbol::intern("panic_unwind"),
            PanicStrategy::Abort => Symbol::intern("panic_abort"),
        };
        info!("panic runtime not found -- loading {}", name);

        let (cnum, data) = self.resolve_crate(name, DUMMY_SP, DepKind::Implicit, None);

        // Sanity check the loaded crate to ensure it is indeed a panic runtime
        // and the panic strategy is indeed what we thought it was.
        if !data.root.panic_runtime {
            self.sess.err(&format!("the crate `{}` is not a panic runtime",
                                   name));
        }
        if data.root.panic_strategy != desired_strategy {
            self.sess.err(&format!("the crate `{}` does not have the panic \
                                    strategy `{}`",
                                   name, desired_strategy.desc()));
        }

        self.sess.injected_panic_runtime.set(Some(cnum));
        self.inject_dependency_if(cnum, "a panic runtime",
                                  &|data| data.root.needs_panic_runtime);
    }

    fn inject_sanitizer_runtime(&mut self) {
        if let Some(ref sanitizer) = self.sess.opts.debugging_opts.sanitizer {
            // Sanitizers can only be used on some tested platforms with
            // executables linked to `std`
            const ASAN_SUPPORTED_TARGETS: &[&str] = &["x86_64-unknown-linux-gnu",
                                                      "x86_64-apple-darwin"];
            const TSAN_SUPPORTED_TARGETS: &[&str] = &["x86_64-unknown-linux-gnu",
                                                      "x86_64-apple-darwin"];
            const LSAN_SUPPORTED_TARGETS: &[&str] = &["x86_64-unknown-linux-gnu"];
            const MSAN_SUPPORTED_TARGETS: &[&str] = &["x86_64-unknown-linux-gnu"];

            let supported_targets = match *sanitizer {
                Sanitizer::Address => ASAN_SUPPORTED_TARGETS,
                Sanitizer::Thread => TSAN_SUPPORTED_TARGETS,
                Sanitizer::Leak => LSAN_SUPPORTED_TARGETS,
                Sanitizer::Memory => MSAN_SUPPORTED_TARGETS,
            };
            if !supported_targets.contains(&&*self.sess.opts.target_triple.triple()) {
                self.sess.err(&format!("{:?}Sanitizer only works with the `{}` target",
                    sanitizer,
                    supported_targets.join("` or `")
                ));
                return
            }

            // firstyear 2017 - during testing I was unable to access an OSX machine
            // to make this work on different crate types. As a result, today I have
            // only been able to test and support linux as a target.
            if self.sess.opts.target_triple.triple() == "x86_64-unknown-linux-gnu" {
                if !self.sess.crate_types.borrow().iter().all(|ct| {
                    match *ct {
                        // Link the runtime
                        config::CrateType::Staticlib |
                        config::CrateType::Executable => true,
                        // This crate will be compiled with the required
                        // instrumentation pass
                        config::CrateType::Rlib |
                        config::CrateType::Dylib |
                        config::CrateType::Cdylib =>
                            false,
                        _ => {
                            self.sess.err(&format!("Only executables, staticlibs, \
                                cdylibs, dylibs and rlibs can be compiled with \
                                `-Z sanitizer`"));
                            false
                        }
                    }
                }) {
                    return
                }
            } else {
                if !self.sess.crate_types.borrow().iter().all(|ct| {
                    match *ct {
                        // Link the runtime
                        config::CrateType::Executable => true,
                        // This crate will be compiled with the required
                        // instrumentation pass
                        config::CrateType::Rlib => false,
                        _ => {
                            self.sess.err(&format!("Only executables and rlibs can be \
                                                    compiled with `-Z sanitizer`"));
                            false
                        }
                    }
                }) {
                    return
                }
            }

            let mut uses_std = false;
            self.cstore.iter_crate_data(|_, data| {
                if data.root.name == sym::std {
                    uses_std = true;
                }
            });

            if uses_std {
                let name = Symbol::intern(match sanitizer {
                    Sanitizer::Address => "rustc_asan",
                    Sanitizer::Leak => "rustc_lsan",
                    Sanitizer::Memory => "rustc_msan",
                    Sanitizer::Thread => "rustc_tsan",
                });
                info!("loading sanitizer: {}", name);

                let data = self.resolve_crate(name, DUMMY_SP, DepKind::Explicit, None).1;

                // Sanity check the loaded crate to ensure it is indeed a sanitizer runtime
                if !data.root.sanitizer_runtime {
                    self.sess.err(&format!("the crate `{}` is not a sanitizer runtime",
                                           name));
                }
            } else {
                self.sess.err("Must link std to be compiled with `-Z sanitizer`");
            }
        }
    }

    fn inject_profiler_runtime(&mut self) {
        if self.sess.opts.debugging_opts.profile ||
           self.sess.opts.cg.profile_generate.enabled()
        {
            info!("loading profiler");

            let name = Symbol::intern("profiler_builtins");
            let data = self.resolve_crate(name, DUMMY_SP, DepKind::Implicit, None).1;

            // Sanity check the loaded crate to ensure it is indeed a profiler runtime
            if !data.root.profiler_runtime {
                self.sess.err(&format!("the crate `profiler_builtins` is not \
                                        a profiler runtime"));
            }
        }
    }

    fn inject_allocator_crate(&mut self, krate: &ast::Crate) {
        let has_global_allocator = match &*global_allocator_spans(krate) {
            [span1, span2, ..] => {
                self.sess.struct_span_err(*span2, "cannot define multiple global allocators")
                         .span_note(*span1, "the previous global allocator is defined here").emit();
                true
            }
            spans => !spans.is_empty()
        };
        self.sess.has_global_allocator.set(has_global_allocator);

        // Check to see if we actually need an allocator. This desire comes
        // about through the `#![needs_allocator]` attribute and is typically
        // written down in liballoc.
        let mut needs_allocator = attr::contains_name(&krate.attrs,
                                                      sym::needs_allocator);
        self.cstore.iter_crate_data(|_, data| {
            needs_allocator = needs_allocator || data.root.needs_allocator;
        });
        if !needs_allocator {
            self.sess.allocator_kind.set(None);
            return
        }

        // At this point we've determined that we need an allocator. Let's see
        // if our compilation session actually needs an allocator based on what
        // we're emitting.
        let all_rlib = self.sess.crate_types.borrow()
            .iter()
            .all(|ct| {
                match *ct {
                    config::CrateType::Rlib => true,
                    _ => false,
                }
            });
        if all_rlib {
            self.sess.allocator_kind.set(None);
            return
        }

        // Ok, we need an allocator. Not only that but we're actually going to
        // create an artifact that needs one linked in. Let's go find the one
        // that we're going to link in.
        //
        // First up we check for global allocators. Look at the crate graph here
        // and see what's a global allocator, including if we ourselves are a
        // global allocator.
        let mut global_allocator = if has_global_allocator {
            Some(None)
        } else {
            None
        };
        self.cstore.iter_crate_data(|_, data| {
            if !data.root.has_global_allocator {
                return
            }
            match global_allocator {
                Some(Some(other_crate)) => {
                    self.sess.err(&format!("the `#[global_allocator]` in {} \
                                            conflicts with this global \
                                            allocator in: {}",
                                           other_crate,
                                           data.root.name));
                }
                Some(None) => {
                    self.sess.err(&format!("the `#[global_allocator]` in this \
                                            crate conflicts with global \
                                            allocator in: {}", data.root.name));
                }
                None => global_allocator = Some(Some(data.root.name)),
            }
        });
        if global_allocator.is_some() {
            self.sess.allocator_kind.set(Some(AllocatorKind::Global));
            return
        }

        // Ok we haven't found a global allocator but we still need an
        // allocator. At this point our allocator request is typically fulfilled
        // by the standard library, denoted by the `#![default_lib_allocator]`
        // attribute.
        let mut has_default = attr::contains_name(&krate.attrs, sym::default_lib_allocator);
        self.cstore.iter_crate_data(|_, data| {
            if data.root.has_default_lib_allocator {
                has_default = true;
            }
        });

        if !has_default {
            self.sess.err("no global memory allocator found but one is \
                           required; link to std or \
                           add `#[global_allocator]` to a static item \
                           that implements the GlobalAlloc trait.");
        }
        self.sess.allocator_kind.set(Some(AllocatorKind::DefaultLib));
    }

    fn inject_dependency_if(&self,
                            krate: CrateNum,
                            what: &str,
                            needs_dep: &dyn Fn(&cstore::CrateMetadata) -> bool) {
        // don't perform this validation if the session has errors, as one of
        // those errors may indicate a circular dependency which could cause
        // this to stack overflow.
        if self.sess.has_errors() {
            return
        }

        // Before we inject any dependencies, make sure we don't inject a
        // circular dependency by validating that this crate doesn't
        // transitively depend on any crates satisfying `needs_dep`.
        for dep in self.cstore.crate_dependencies_in_rpo(krate) {
            let data = self.cstore.get_crate_data(dep);
            if needs_dep(&data) {
                self.sess.err(&format!("the crate `{}` cannot depend \
                                        on a crate that needs {}, but \
                                        it depends on `{}`",
                                       self.cstore.get_crate_data(krate).root.name,
                                       what,
                                       data.root.name));
            }
        }

        // All crates satisfying `needs_dep` do not explicitly depend on the
        // crate provided for this compile, but in order for this compilation to
        // be successfully linked we need to inject a dependency (to order the
        // crates on the command line correctly).
        self.cstore.iter_crate_data(|cnum, data| {
            if !needs_dep(data) {
                return
            }

            info!("injecting a dep from {} to {}", cnum, krate);
            data.dependencies.borrow_mut().push(krate);
        });
    }
}

impl<'a> CrateLoader<'a> {
    pub fn postprocess(&mut self, krate: &ast::Crate) {
        self.inject_sanitizer_runtime();
        self.inject_profiler_runtime();
        self.inject_allocator_crate(krate);
        self.inject_panic_runtime(krate);

        if log_enabled!(log::Level::Info) {
            dump_crates(&self.cstore);
        }
    }

    pub fn process_extern_crate(
        &mut self, item: &ast::Item, definitions: &Definitions,
    ) -> CrateNum {
        match item.kind {
            ast::ItemKind::ExternCrate(orig_name) => {
                debug!("resolving extern crate stmt. ident: {} orig_name: {:?}",
                       item.ident, orig_name);
                let name = match orig_name {
                    Some(orig_name) => {
                        crate::validate_crate_name(Some(self.sess), &orig_name.as_str(),
                                            Some(item.span));
                        orig_name
                    }
                    None => item.ident.name,
                };
                let dep_kind = if attr::contains_name(&item.attrs, sym::no_link) {
                    DepKind::UnexportedMacrosOnly
                } else {
                    DepKind::Explicit
                };

                let cnum = self.resolve_crate(name, item.span, dep_kind, None).0;

                let def_id = definitions.opt_local_def_id(item.id).unwrap();
                let path_len = definitions.def_path(def_id.index).data.len();
                self.update_extern_crate(
                    cnum,
                    ExternCrate {
                        src: ExternCrateSource::Extern(def_id),
                        span: item.span,
                        path_len,
                        dependency_of: LOCAL_CRATE,
                    },
                    &mut FxHashSet::default(),
                );
                self.cstore.add_extern_mod_stmt_cnum(item.id, cnum);
                cnum
            }
            _ => bug!(),
        }
    }

    pub fn process_path_extern(
        &mut self,
        name: Symbol,
        span: Span,
    ) -> CrateNum {
        let cnum = self.resolve_crate(name, span, DepKind::Explicit, None).0;

        self.update_extern_crate(
            cnum,
            ExternCrate {
                src: ExternCrateSource::Path,
                span,
                // to have the least priority in `update_extern_crate`
                path_len: usize::max_value(),
                dependency_of: LOCAL_CRATE,
            },
            &mut FxHashSet::default(),
        );

        cnum
    }

    pub fn maybe_process_path_extern(
        &mut self,
        name: Symbol,
        span: Span,
    ) -> Option<CrateNum> {
        let cnum = self.maybe_resolve_crate(name, span, DepKind::Explicit, None).ok()?.0;

        self.update_extern_crate(
            cnum,
            ExternCrate {
                src: ExternCrateSource::Path,
                span,
                // to have the least priority in `update_extern_crate`
                path_len: usize::max_value(),
                dependency_of: LOCAL_CRATE,
            },
            &mut FxHashSet::default(),
        );

        Some(cnum)
    }
}
