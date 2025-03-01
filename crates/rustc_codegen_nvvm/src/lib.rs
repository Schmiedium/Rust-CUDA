#![feature(rustc_private)]
// crate is perma-unstable because of rustc_private so might as well
// make our lives a lot easier for llvm ffi with this. And since rustc's core infra
// relies on it its almost guaranteed to not be removed/broken
#![feature(extern_types)]

extern crate rustc_arena;
extern crate rustc_ast;
extern crate rustc_attr;
extern crate rustc_codegen_llvm;
extern crate rustc_codegen_ssa;
extern crate rustc_data_structures;
extern crate rustc_driver;
extern crate rustc_errors;
extern crate rustc_fs_util;
extern crate rustc_hash;
extern crate rustc_hir;
extern crate rustc_index;
extern crate rustc_interface;
extern crate rustc_metadata;
extern crate rustc_middle;
extern crate rustc_query_system;
extern crate rustc_session;
extern crate rustc_span;
extern crate rustc_target;

mod abi;
mod allocator;
mod asm;
mod attributes;
mod back;
mod builder;
mod const_ty;
mod consts;
mod context;
mod ctx_intrinsics;
mod debug_info;
mod init;
mod int_replace;
mod intrinsic;
mod link;
mod llvm;
mod lto;
mod mono_item;
mod nvvm;
mod override_fns;
mod target;
mod ty;

use abi::readjust_fn_abi;
use back::target_machine_factory;
use lto::ThinBuffer;
use rustc_codegen_ssa::{
    back::{
        lto::{LtoModuleCodegen, SerializedModule, ThinModule},
        write::{CodegenContext, ModuleConfig, OngoingCodegen},
    },
    traits::{CodegenBackend, ExtraBackendMethods, WriteBackendMethods},
    CodegenResults, CompiledModule, ModuleCodegen,
};
use rustc_errors::FatalError;
use rustc_hash::FxHashMap;
use rustc_metadata::EncodedMetadata;
use rustc_middle::query;
use rustc_middle::{
    dep_graph::{WorkProduct, WorkProductId},
    ty::TyCtxt,
};
use rustc_session::{Session, config::OutputFilenames};
use tracing::debug;

use std::ffi::CString;

// codegen dylib entrypoint
#[no_mangle]
pub fn __rustc_codegen_backend() -> Box<dyn CodegenBackend> {
    let _ = std::panic::take_hook();
    Box::new(NvvmCodegenBackend::default())
}

#[derive(Default, Clone)]
pub struct NvvmCodegenBackend(());

unsafe impl Send for NvvmCodegenBackend {}
unsafe impl Sync for NvvmCodegenBackend {}

impl CodegenBackend for NvvmCodegenBackend {
    fn init(&self, sess: &Session) {
        let filter = tracing_subscriber::EnvFilter::from_env("NVVM_LOG");
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .compact()
            .finish();

        tracing::subscriber::set_global_default(subscriber).expect("no default subscriber");
        init::init(sess);
    }
    fn metadata_loader(&self) -> Box<MetadataLoaderDyn> {
        Box::new(link::NvvmMetadataLoader)
    }

    fn provide(&self, providers: &mut rustc_middle::util::Providers) {
        providers.fn_abi_of_fn_ptr = |tcx, key| {
            let result = (rustc_interface::DEFAULT_QUERY_PROVIDERS.fn_abi_of_fn_ptr)(tcx, key);
            Ok(readjust_fn_abi(tcx, result?))
        };
        providers.fn_abi_of_instance = |tcx, key| {
            let result = (rustc_interface::DEFAULT_QUERY_PROVIDERS.fn_abi_of_instance)(tcx, key);
            Ok(readjust_fn_abi(tcx, result?))
        };
    }
    fn provide_extern(&self, _providers: &mut query::ExternProviders) {}

    fn codegen_crate(
        &self,
        tcx: TyCtxt<'_>,
        metadata: EncodedMetadata,
        need_metadata_module: bool,
    ) -> Box<dyn std::any::Any> {
        debug!("Codegen crate");
        Box::new(rustc_codegen_ssa::base::codegen_crate(
            NvvmCodegenBackend::default(),
            tcx,
            String::new(),
            metadata,
            need_metadata_module,
        ))
    }

    fn join_codegen(
        &self,
        ongoing_codegen: Box<dyn std::any::Any>,
        sess: &Session,
        outputs: &rustc_session::config::OutputFilenames,
    ) -> Result<(CodegenResults, FxHashMap<WorkProductId, WorkProduct>), ErrorReported> {
        debug!("Join codegen");
        let (codegen_results, work_products) = ongoing_codegen
            .downcast::<OngoingCodegen<Self>>()
            .expect("Expected OngoingCodegen, found Box<Any>")
            .join(sess);

        sess.compile_status()?;

        Ok((codegen_results, work_products))
    }

    fn link(
        &self,
        sess: &rustc_session::Session,
        codegen_results: rustc_codegen_ssa::CodegenResults,
        outputs: &OutputFilenames,
    ) -> Result<(), rustc_errors::ErrorReported> {
        link::link(
            sess,
            &codegen_results,
            outputs,
            &codegen_results.crate_info.local_crate_name.as_str(),
        );
        Ok(())
    }
    fn locale_resource(&self) -> &'static str { todo!() }
}

impl WriteBackendMethods for NvvmCodegenBackend {
    type Module = LlvmMod;
    type ModuleBuffer = lto::ModuleBuffer;
    type Context = llvm::Context;
    type TargetMachine = &'static mut llvm::TargetMachine;
    type ThinData = ();
    type ThinBuffer = ThinBuffer;
    type TargetMachineError = ();

    fn run_link(
        _cgcx: &CodegenContext<Self>,
        _diag_handler: &Handler,
        _modules: Vec<ModuleCodegen<Self::Module>>,
    ) -> Result<ModuleCodegen<Self::Module>, FatalError> {
        // TODO(Rdambrosio016):
        // we can probably call the llvm codegen to do this, but cgcx
        // is a codegen context of NvvmCodegenBackend not LlvmCodegenBackend
        // and to make a new cgcx we need to make a new LlvmCodegenBackend which
        // cannot be done through the API currently
        todo!();
    }

    fn run_fat_lto(
        _: &CodegenContext<Self>,
        _: Vec<FatLTOInput<Self>>,
        _: Vec<(SerializedModule<Self::ModuleBuffer>, WorkProduct)>,
    ) -> Result<LtoModuleCodegen<Self>, FatalError> {
        todo!()
    }

    fn run_thin_lto(
        cgcx: &CodegenContext<Self>,
        modules: Vec<(String, Self::ThinBuffer)>,
        cached_modules: Vec<(SerializedModule<Self::ModuleBuffer>, WorkProduct)>,
    ) -> Result<(Vec<LtoModuleCodegen<Self>>, Vec<WorkProduct>), FatalError> {
        lto::run_thin(cgcx, modules, cached_modules)
    }

    fn print_pass_timings(&self) {
        // Not applicable, nvvm doesnt expose pass timing info, maybe we could print llvm pass stuff here.
    }

    unsafe fn optimize(
        cgcx: &CodegenContext<Self>,
        diag_handler: &Handler,
        module: &ModuleCodegen<Self::Module>,
        config: &ModuleConfig,
    ) -> Result<(), FatalError> {
        back::optimize(cgcx, diag_handler, module, config)
    }

    unsafe fn optimize_thin(
        cgcx: &CodegenContext<Self>,
        thin_module: ThinModule<Self>,
    ) -> Result<ModuleCodegen<Self::Module>, FatalError> {
        lto::optimize_thin(cgcx, thin_module)
    }

    unsafe fn codegen(
        cgcx: &CodegenContext<Self>,
        diag_handler: &Handler,
        module: ModuleCodegen<Self::Module>,
        config: &ModuleConfig,
    ) -> Result<CompiledModule, FatalError> {
        back::codegen(cgcx, diag_handler, module, config)
    }

    fn prepare_thin(module: ModuleCodegen<Self::Module>, want_summary: bool) -> (String, Self::ThinBuffer) {
        debug!("Prepare thin");
        unsafe {
            (
                module.name,
                lto::ThinBuffer::new(module.module_llvm.llmod.as_ref().unwrap()),
            )
        }
    }

    fn serialize_module(module: ModuleCodegen<Self::Module>) -> (String, Self::ModuleBuffer) {
        debug!("Serializing module");
        unsafe {
            (
                module.name,
                lto::ModuleBuffer::new(module.module_llvm.llmod.as_ref().unwrap()),
            )
        }
    }

    fn run_lto_pass_manager(
        _: &CodegenContext<Self>,
        _: &ModuleCodegen<Self::Module>,
        _: &ModuleConfig,
        _: bool,
    ) -> Result<(), FatalError> {
        todo!()
    }
    fn print_statistics(&self) { todo!() }
    fn optimize_fat(_: &CodegenContext<Self>, _: &mut ModuleCodegen<<Self as rustc_codegen_ssa::traits::WriteBackendMethods>::Module>) -> Result<(), FatalError> { todo!() }
    fn autodiff(_: &CodegenContext<Self>, _: TyCtxt<'_>, _: &ModuleCodegen<<Self as rustc_codegen_ssa::traits::WriteBackendMethods>::Module>, _: Vec<AutoDiffItem>, _: &ModuleConfig) -> Result<(), FatalError> { todo!() }
}

impl ExtraBackendMethods for NvvmCodegenBackend {
    fn new_metadata(&self, _sess: TyCtxt<'_>, mod_name: &str) -> Self::Module {
        LlvmMod::new(mod_name)
    }

    fn write_compressed_metadata<'tcx>(
        &self,
        _tcx: TyCtxt<'tcx>,
        _metadata: &EncodedMetadata,
        _llvm_module: &mut Self::Module,
    ) {
        todo!()
    }

    fn codegen_allocator<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        _module_name: &str,
        kind: rustc_ast::expand::allocator::AllocatorKind,
        alloc_error_handler_kind: rustc_ast::expand::allocator::AllocatorKind,
     ) -> LlvmMod {
        unsafe { allocator::codegen(tcx, mods, kind, has_alloc_error_handler) }
    }

    fn compile_codegen_unit(
        &self,
        tcx: TyCtxt<'_>,
        cgu_name: rustc_span::Symbol,
    ) -> (rustc_codegen_ssa::ModuleCodegen<Self::Module>, u64) {
        back::compile_codegen_unit(tcx, cgu_name)
    }

    fn target_machine_factory(
        &self,
        sess: &Session,
        opt_level: rustc_session::config::OptLevel,
        target_features: &[String],
    ) -> rustc_codegen_ssa::back::write::TargetMachineFactoryFn<Self> {
        target_machine_factory(sess, opt_level)
    }

    fn target_cpu<'b>(&self, _sess: &'b Session) -> &'b str {
        todo!()
    }

    fn tune_cpu<'b>(&self, _sess: &'b Session) -> Option<&'b str> {
        todo!()
    }
}

/// Create the LLVM module for the rest of the compilation, this houses
/// the LLVM bitcode we then add to the NVVM program and feed to libnvvm.
/// LLVM's codegen is never actually called.
pub(crate) unsafe fn create_module<'ll>(
    llcx: &'ll llvm::Context,
    mod_name: &str,
) -> &'ll llvm::Module {
    debug!("Creating llvm module with name `{}`", mod_name);
    let mod_name = CString::new(mod_name).expect("nul in module name");
    let llmod = llvm::LLVMModuleCreateWithNameInContext(mod_name.as_ptr(), llcx);

    let data_layout = CString::new(target::DATA_LAYOUT).unwrap();
    llvm::LLVMSetDataLayout(llmod, data_layout.as_ptr());

    let target = CString::new(target::TARGET_TRIPLE).unwrap();
    llvm::LLVMSetTarget(llmod, target.as_ptr());

    llmod
}

/// Wrapper over raw llvm structures
pub struct LlvmMod {
    llcx: &'static mut llvm::Context,
    llmod: *const llvm::Module,
}

unsafe impl Send for LlvmMod {}
unsafe impl Sync for LlvmMod {}

impl LlvmMod {
    pub fn new(name: &str) -> Self {
        unsafe {
            // TODO(RDambrosio016): does shouldDiscardNames affect NVVM at all?
            let llcx = llvm::LLVMRustContextCreate(false);
            let llmod = create_module(llcx, name) as *const _;
            LlvmMod { llcx, llmod }
        }
    }
}

impl Drop for LlvmMod {
    fn drop(&mut self) {
        unsafe {
            llvm::LLVMContextDispose(&mut *(self.llcx as *mut _));
        }
    }
}
