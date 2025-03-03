use crate::abi::FnAbiLlvmExt;
use crate::attributes::{self, NvvmAttributes, Symbols};
use crate::debug_info::{self, compile_unit_metadata, CrateDebugContext};
use crate::llvm::{self, BasicBlock, Type, Value};
use crate::{target, LlvmMod};
use nvvm::NvvmOption;
use rustc_codegen_ssa::traits::DerivedTypeCodegenMethods;
use rustc_codegen_ssa::traits::{
    BackendTypes, BaseTypeCodegenMethods, ConstCodegenMethods, MiscCodegenMethods,
    TypeMembershipCodegenMethods,
};
use rustc_data_structures::base_n;
use rustc_hash::FxHashMap;
use rustc_middle::dep_graph::DepContext;
use rustc_middle::ty::layout::{
    FnAbiError, FnAbiOf, FnAbiRequest, HasTyCtxt, LayoutError, TyAndLayout,
};
use rustc_middle::ty::layout::{FnAbiOfHelpers, HasTypingEnv, LayoutOfHelpers};
use rustc_middle::ty::Ty;
use rustc_middle::{bug, span_bug, ty};
use rustc_middle::{
    mir::mono::CodegenUnit,
    ty::{Instance, PolyExistentialTraitRef, TyCtxt},
};
use rustc_session::config::DebugInfo;
use rustc_session::Session;
use rustc_span::{Span, Symbol};
use rustc_target::abi::call::FnAbi;
use rustc_target::abi::{
    AddressSpace, HasDataLayout, PointeeInfo, Size, TargetDataLayout, VariantIdx,
};
use rustc_target::spec::{HasTargetSpec, Target};
use std::cell::{Cell, RefCell};
use std::ffi::CStr;
use std::hash::BuildHasherDefault;
use std::os::raw::c_char;
use std::path::PathBuf;
use std::ptr::null;
use std::str::FromStr;
use tracing::{debug, trace};

pub(crate) struct CodegenCx<'ll, 'tcx> {
    pub tcx: TyCtxt<'tcx>,
    pub check_overflow: bool,

    pub llmod: &'ll llvm::Module,
    pub llcx: &'ll llvm::Context,
    pub codegen_unit: &'tcx CodegenUnit<'tcx>,

    /// Map of MIR functions to LLVM function values
    pub instances: RefCell<FxHashMap<Instance<'tcx>, &'ll Value>>,
    /// A cache of the generated vtables for trait objects
    pub vtables: RefCell<FxHashMap<(Ty<'tcx>, Option<PolyExistentialTraitRef<'tcx>>), &'ll Value>>,
    /// A cache of constant strings and their values
    pub const_cstr_cache: RefCell<FxHashMap<Symbol, &'ll Value>>,
    /// A map of functions which have parameters at specific indices replaced with an int-remapped type.
    /// such as i128 --> <2 x i64>
    #[allow(clippy::type_complexity)]
    pub remapped_integer_args:
        RefCell<FxHashMap<&'ll Type, (Option<&'ll Type>, Vec<(usize, &'ll Type)>)>>,

    /// Cache of emitted const globals (value -> global)
    pub const_globals: RefCell<FxHashMap<&'ll Value, &'ll Value>>,

    /// List of globals for static variables which need to be passed to the
    /// LLVM function ReplaceAllUsesWith (RAUW) when codegen is complete.
    /// (We have to make sure we don't invalidate any Values referring
    /// to constants.)
    pub statics_to_rauw: RefCell<Vec<(&'ll Value, &'ll Value)>>,

    /// Statics that will be placed in the llvm.used variable
    /// See <http://llvm.org/docs/LangRef.html#the-llvm-used-global-variable> for details
    pub used_statics: RefCell<Vec<&'ll Value>>,

    /// Statics that will be placed in the llvm.compiler.used variable
    /// See <https://llvm.org/docs/LangRef.html#the-llvm-compiler-used-global-variable> for details
    pub compiler_used_statics: RefCell<Vec<&'ll Value>>,

    pub lltypes: RefCell<FxHashMap<(Ty<'tcx>, Option<VariantIdx>), &'ll Type>>,
    pub scalar_lltypes: RefCell<FxHashMap<Ty<'tcx>, &'ll Type>>,
    pub pointee_infos: RefCell<FxHashMap<(Ty<'tcx>, Size), Option<PointeeInfo>>>,
    pub isize_ty: &'ll Type,

    pub dbg_cx: Option<debug_info::CrateDebugContext<'ll, 'tcx>>,

    /// A map of the intrinsics we actually declared for usage.
    pub(crate) intrinsics: RefCell<FxHashMap<String, &'ll Value>>,
    /// A map of the intrinsics available but not yet declared.
    pub(crate) intrinsics_map: RefCell<FxHashMap<&'static str, (Vec<&'ll Type>, &'ll Type)>>,

    local_gen_sym_counter: Cell<usize>,

    nvptx_data_layout: TargetDataLayout,
    nvptx_target: Target,

    /// empty eh_personality function
    eh_personality: &'ll Value,

    pub symbols: Symbols,
    pub codegen_args: CodegenArgs,
    // the value of the last call instruction. Needed for return type remapping.
    pub last_call_llfn: Cell<Option<&'ll Value>>,
}

impl<'ll, 'tcx> CodegenCx<'ll, 'tcx> {
    pub(crate) fn new(
        tcx: TyCtxt<'tcx>,
        codegen_unit: &'tcx CodegenUnit<'tcx>,
        llvm_module: &'ll LlvmMod,
    ) -> Self {
        debug!("Creating new CodegenCx");
        let check_overflow = tcx.sess.overflow_checks();
        let (llcx, llmod) = (&*llvm_module.llcx, unsafe {
            llvm_module.llmod.as_ref().unwrap()
        });

        let isize_ty = Type::ix_llcx(llcx, target::POINTER_WIDTH as u64);
        // the eh_personality function doesnt make sense on the GPU, but we still need to give
        // rustc something, so we just give it an empty function
        let eh_personality = unsafe {
            let void = llvm::LLVMVoidTypeInContext(llcx);
            let llfnty = llvm::LLVMFunctionType(void, null(), 0, llvm::False);
            let name = "__rust_eh_personality";
            llvm::LLVMRustGetOrInsertFunction(llmod, name.as_ptr().cast(), name.len(), llfnty)
        };

        let dbg_cx = if tcx.sess.opts.debuginfo != DebugInfo::None {
            let dctx = CrateDebugContext::new(llmod);
            compile_unit_metadata(tcx, &codegen_unit.name().as_str(), &dctx);
            Some(dctx)
        } else {
            None
        };

        let mut cx = CodegenCx {
            tcx,
            check_overflow,
            llmod,
            llcx,
            codegen_unit,
            instances: Default::default(),
            vtables: Default::default(),
            const_cstr_cache: Default::default(),
            remapped_integer_args: Default::default(),
            const_globals: Default::default(),
            statics_to_rauw: RefCell::new(Vec::new()),
            used_statics: RefCell::new(Vec::new()),
            compiler_used_statics: RefCell::new(Vec::new()),
            lltypes: Default::default(),
            scalar_lltypes: Default::default(),
            pointee_infos: Default::default(),
            isize_ty,
            intrinsics: Default::default(),
            intrinsics_map: RefCell::new(FxHashMap::with_capacity_and_hasher(
                // ~319 libdevice intrinsics plus some headroom for llvm
                350,
                BuildHasherDefault::default(),
            )),
            local_gen_sym_counter: Cell::new(0),
            nvptx_data_layout: TargetDataLayout::parse(&target::target()).unwrap(),
            nvptx_target: target::target(),
            eh_personality,
            symbols: Symbols {
                nvvm_internal: Symbol::intern("nvvm_internal"),
                kernel: Symbol::intern("kernel"),
                addrspace: Symbol::intern("addrspace"),
            },
            dbg_cx,
            codegen_args: CodegenArgs::from_session(tcx.sess()),
            last_call_llfn: Cell::new(None),
        };
        cx.build_intrinsics_map();
        cx
    }

    pub(crate) fn fatal(&self, msg: &str) -> ! {
        self.tcx.sess.fatal(msg)
    }

    // im lazy i know
    pub(crate) fn unsupported(&self, thing: &str) -> ! {
        self.fatal(&format!("{} is unsupported", thing))
    }

    fn create_used_variable_impl(&self, name: *const c_char, values: &[&'ll Value]) {
        let section = "llvm.metadata\0".as_ptr().cast();
        let array = self.const_array(self.type_ptr_to(self.type_i8()), values);

        unsafe {
            trace!(
                "Creating LLVM used variable with name `{}` and values:\n{:#?}",
                CStr::from_ptr(name).to_str().unwrap(),
                values
            );
            let g = llvm::LLVMAddGlobal(self.llmod, self.val_ty(array), name);
            llvm::LLVMSetInitializer(g, array);
            llvm::LLVMRustSetLinkage(g, llvm::Linkage::AppendingLinkage);
            llvm::LLVMSetSection(g, section);
        }
    }
}

fn sanitize_global_ident(name: &str) -> String {
    name.replace(".", "$")
}

impl<'ll, 'tcx> MiscCodegenMethods<'tcx> for CodegenCx<'ll, 'tcx> {
    fn vtables(
        &self,
    ) -> &RefCell<FxHashMap<(Ty<'tcx>, Option<PolyExistentialTraitRef<'tcx>>), &'ll Value>> {
        &self.vtables
    }

    fn get_fn(&self, instance: Instance<'tcx>) -> &'ll Value {
        self.get_fn(instance)
    }

    fn get_fn_addr(&self, instance: Instance<'tcx>) -> &'ll Value {
        self.get_fn(instance)
    }

    fn eh_personality(&self) -> &'ll Value {
        self.eh_personality
    }

    fn sess(&self) -> &Session {
        self.tcx.sess
    }

    fn check_overflow(&self) -> bool {
        self.check_overflow
    }

    fn codegen_unit(&self) -> &'tcx CodegenUnit<'tcx> {
        self.codegen_unit
    }

    fn used_statics(&self) -> &RefCell<Vec<&'ll Value>> {
        &self.used_statics
    }

    fn create_used_variable(&self) {
        self.create_used_variable_impl("llvm.used\0".as_ptr().cast(), &*self.used_statics.borrow());
    }

    fn create_compiler_used_variable(&self) {
        self.create_used_variable_impl(
            "llvm.compiler.used\0".as_ptr().cast(),
            &*self.compiler_used_statics.borrow(),
        );
    }

    fn declare_c_main(
        &self,
        _fn_type: <CodegenCx<'ll, 'tcx> as rustc_codegen_ssa::traits::BackendTypes>::Type,
    ) -> Option<<CodegenCx<'ll, 'tcx> as rustc_codegen_ssa::traits::BackendTypes>::Function> {
        // no point for gpu kernels
        None
    }

    fn apply_target_cpu_attr(
        &self,
        _llfn: <CodegenCx<'ll, 'tcx> as rustc_codegen_ssa::traits::BackendTypes>::Function,
    ) {
        // no point if we are running on the gpu ;)
    }

    fn compiler_used_statics(
        &self,
    ) -> &RefCell<Vec<<CodegenCx<'ll, 'tcx> as rustc_codegen_ssa::traits::BackendTypes>::Value>>
    {
        &self.compiler_used_statics
    }

    fn set_frame_pointer_type(
        &self,
        _llfn: <CodegenCx<'ll, 'tcx> as rustc_codegen_ssa::traits::BackendTypes>::Function,
    ) {
    }
}

impl<'ll, 'tcx> CodegenCx<'ll, 'tcx> {
    /// Computes the address space for a static.
    pub fn static_addrspace(&self, instance: Instance<'tcx>) -> AddressSpace {
        let ty = instance.ty(self.tcx, ty::ParamEnv::reveal_all());
        let is_mutable = self.tcx().is_mutable_static(instance.def_id());
        let attrs = self.tcx.get_attrs(instance.def_id());
        let nvvm_attrs = NvvmAttributes::parse(self, attrs);

        if let Some(addr) = nvvm_attrs.addrspace {
            return AddressSpace(addr as u32);
        }

        if !is_mutable && self.type_is_freeze(ty) {
            AddressSpace(4)
        } else {
            AddressSpace::DATA
        }
    }

    /// Declare a global value, returns the existing value if it was already declared.
    pub fn declare_global(
        &self,
        name: &str,
        ty: &'ll Type,
        address_space: AddressSpace,
    ) -> &'ll Value {
        // NVVM doesnt allow `.` inside of globals, this should be sound, at worst it should result in an nvvm error if something goes wrong.
        let name = sanitize_global_ident(name);
        trace!("Declaring global `{}`", name);
        unsafe {
            llvm::LLVMRustGetOrInsertGlobal(
                self.llmod,
                name.as_ptr().cast(),
                name.len(),
                ty,
                address_space.0,
            )
        }
    }

    /// Declare a function. All functions use the default ABI, NVVM ignores any calling convention markers.
    /// All functions calls are generated according to the PTX calling convention.
    /// <https://docs.nvidia.com/cuda/nvvm-ir-spec/index.html#calling-conventions>
    pub fn declare_fn(
        &self,
        name: &str,
        ty: &'ll Type,
        fn_abi: Option<&FnAbi<'tcx, Ty<'tcx>>>,
    ) -> &'ll Value {
        let llfn = unsafe {
            llvm::LLVMRustGetOrInsertFunction(self.llmod, name.as_ptr().cast(), name.len(), ty)
        };

        trace!("Declaring function `{}` with ty `{:?}`", name, ty);

        // TODO(RDambrosio016): we should probably still generate accurate calling conv for functions
        // just to make it easier to debug IR and/or make it more compatible with compiling using llvm
        llvm::SetUnnamedAddress(llfn, llvm::UnnamedAddr::Global);
        if let Some(abi) = fn_abi {
            abi.apply_attrs_llfn(self, llfn);
        }
        attributes::default_optimisation_attrs(self.tcx.sess, llfn);
        llfn
    }

    /// Declare a global with an intention to define it.
    ///
    /// Use this function when you intend to define a global. This function will
    /// return `None` if the name already has a definition associated with it. In that
    /// case an error should be reported to the user, because it usually happens due
    /// to user’s fault (e.g., misuse of `#[no_mangle]` or `#[export_name]` attributes).
    pub fn define_global(
        &self,
        name: &str,
        ty: &'ll Type,
        address_space: AddressSpace,
    ) -> Option<&'ll Value> {
        if self.get_defined_value(name).is_some() {
            None
        } else {
            Some(self.declare_global(name, ty, address_space))
        }
    }

    // /// Declare a private global
    // ///
    // /// Use this function when you intend to define a global without a name.
    // pub fn define_private_global(&self, ty: &'ll Type) -> &'ll Value {
    //     println!("Declaring private global with ty `{:?}`", ty);
    //     unsafe { llvm::LLVMRustInsertPrivateGlobal(self.llmod, ty) }
    // }

    /// Gets declared value by name.
    pub fn get_declared_value(&self, name: &str) -> Option<&'ll Value> {
        // NVVM doesnt allow `.` inside of globals, this should be sound, at worst it should result in an llvm/nvvm error if something goes wrong.
        let name = sanitize_global_ident(name);
        trace!("Retrieving value with name `{}`...", name);
        let res =
            unsafe { llvm::LLVMRustGetNamedValue(self.llmod, name.as_ptr().cast(), name.len()) };
        trace!("...Retrieved value: `{:?}`", res);
        res
    }

    /// Gets defined or externally defined (AvailableExternally linkage) value by
    /// name.
    pub fn get_defined_value(&self, name: &str) -> Option<&'ll Value> {
        self.get_declared_value(name).and_then(|val| {
            let declaration = unsafe { llvm::LLVMIsDeclaration(val) != 0 };
            if !declaration {
                Some(val)
            } else {
                None
            }
        })
    }

    pub(crate) fn get_intrinsic(&self, key: &str) -> &'ll Value {
        trace!("Retrieving intrinsic with name `{}`", key);
        if let Some(v) = self.intrinsics.borrow().get(key).cloned() {
            return v;
        }

        self.declare_intrinsic(key)
            .unwrap_or_else(|| bug!("unknown intrinsic '{}'", key))
    }

    pub(crate) fn insert_intrinsic(
        &self,
        name: String,
        args: Option<&[&'ll Type]>,
        ret: &'ll Type,
    ) -> &'ll Value {
        let fn_ty = if let Some(args) = args {
            self.type_func(args, ret)
        } else {
            self.type_variadic_func(&[], ret)
        };
        let f = self.declare_fn(&name, fn_ty, None);
        llvm::SetUnnamedAddress(f, llvm::UnnamedAddr::No);
        self.intrinsics.borrow_mut().insert(name, f);
        f
    }

    pub fn generate_local_symbol_name(&self, prefix: &str) -> String {
        let idx = self.local_gen_sym_counter.get();
        self.local_gen_sym_counter.set(idx + 1);
        // Include a '.' character, so there can be no accidental conflicts with
        // user defined names
        let mut name = String::with_capacity(prefix.len() + 6);
        name.push_str(prefix);
        name.push('.');
        base_n::push_str(idx as u128, base_n::ALPHANUMERIC_ONLY, &mut name);
        name
    }

    //// Codegens a reference to a function/method, monomorphizing and inlining as it goes.
    pub fn get_fn(&self, instance: Instance<'tcx>) -> &'ll Value {
        let tcx = self.tcx;

        assert!(!instance.substs.needs_infer());
        assert!(!instance.substs.has_escaping_bound_vars());
        let sym = tcx.symbol_name(instance).name;

        if let Some(&llfn) = self.instances.borrow().get(&instance) {
            return llfn;
        }

        let abi = self.fn_abi_of_instance(instance, ty::List::empty());

        let llfn = if let Some(llfn) = self.get_declared_value(sym) {
            trace!("Returning existing llfn `{:?}`", llfn);
            let llptrty = abi.ptr_to_llvm_type(self);

            if self.val_ty(llfn) != llptrty {
                trace!(
                    "ptrcasting llfn to different llptrty: `{:?}` --> `{:?}`",
                    llfn,
                    llptrty
                );
                self.const_ptrcast(llfn, llptrty)
            } else {
                llfn
            }
        } else {
            let llfn = self.declare_fn(sym, abi.llvm_type(self), Some(abi));
            attributes::from_fn_attrs(self, llfn, instance);
            let def_id = instance.def_id();

            unsafe {
                llvm::LLVMRustSetLinkage(llfn, llvm::Linkage::ExternalLinkage);

                let is_generic = instance.substs.non_erasable_generics().next().is_some();

                // nvvm ignores visibility styles, but we still make them just in case it will do something
                // with them in the future or we want to use that metadata
                if is_generic {
                    if tcx.sess.opts.share_generics() {
                        if let Some(instance_def_id) = def_id.as_local() {
                            // This is a definition from the current crate. If the
                            // definition is unreachable for downstream crates or
                            // the current crate does not re-export generics, the
                            // definition of the instance will have been declared
                            // as `hidden`.
                            if tcx.is_unreachable_local_definition(instance_def_id)
                                || !tcx.local_crate_exports_generics()
                            {
                                llvm::LLVMRustSetVisibility(llfn, llvm::Visibility::Hidden);
                            }
                        } else {
                            // This is a monomorphization of a generic function
                            // defined in an upstream crate.
                            if instance.upstream_monomorphization(tcx).is_some() {
                                // This is instantiated in another crate. It cannot
                                // be `hidden`.
                            } else {
                                // This is a local instantiation of an upstream definition.
                                // If the current crate does not re-export it
                                // (because it is a C library or an executable), it
                                // will have been declared `hidden`.
                                if !tcx.local_crate_exports_generics() {
                                    llvm::LLVMRustSetVisibility(llfn, llvm::Visibility::Hidden);
                                }
                            }
                        }
                    } else {
                        // When not sharing generics, all instances are in the same
                        // crate and have hidden visibility
                        llvm::LLVMRustSetVisibility(llfn, llvm::Visibility::Hidden);
                    }
                } else {
                    // This is a non-generic function
                    if tcx.is_codegened_item(def_id) {
                        // This is a function that is instantiated in the local crate

                        if def_id.is_local() {
                            // This is function that is defined in the local crate.
                            // If it is not reachable, it is hidden.
                            if !tcx.is_reachable_non_generic(def_id) {
                                llvm::LLVMRustSetVisibility(llfn, llvm::Visibility::Hidden);
                            }
                        } else {
                            // This is a function from an upstream crate that has
                            // been instantiated here. These are always hidden.
                            llvm::LLVMRustSetVisibility(llfn, llvm::Visibility::Hidden);
                        }
                    }
                }
                llfn
            }
        };

        self.instances.borrow_mut().insert(instance, llfn);

        llfn
    }
}

#[derive(Default, Clone)]
pub struct CodegenArgs {
    pub nvvm_options: Vec<NvvmOption>,
    pub override_libm: bool,
    pub final_module_path: Option<PathBuf>,
}

impl CodegenArgs {
    pub fn from_session(sess: &Session) -> Self {
        Self::parse(&sess.opts.cg.llvm_args)
    }

    // we may want to use rustc's own option parsing facilities to have better errors in the future.
    pub fn parse(args: &[String]) -> Self {
        // TODO: replace this with a "proper" arg parser.
        let mut cg_args = Self::default();

        for (idx, arg) in args.iter().enumerate() {
            if let Ok(flag) = NvvmOption::from_str(arg) {
                cg_args.nvvm_options.push(flag);
            } else if arg == "--override-libm" {
                cg_args.override_libm = true;
            } else if arg == "--final-module-path" {
                cg_args.final_module_path = Some(PathBuf::from(
                    args.get(idx + 1).expect("No path for --final-module-path"),
                ));
            }
        }

        cg_args
    }
}

impl<'ll, 'tcx> BackendTypes for CodegenCx<'ll, 'tcx> {
    type Value = &'ll Value;
    type Function = &'ll Value;

    type BasicBlock = &'ll BasicBlock;
    type Type = &'ll Type;
    // not applicable to nvvm, unwinding/exception handling on the gpu
    // doesnt exist.
    type Funclet = ();

    type DIScope = &'ll llvm::DIScope;
    type DILocation = &'ll llvm::DILocation;
    type DIVariable = &'ll llvm::DIVariable;
    type Metadata = &'ll llvm::Metadata;
}

impl<'ll, 'tcx> HasDataLayout for CodegenCx<'ll, 'tcx> {
    fn data_layout(&self) -> &TargetDataLayout {
        &self.nvptx_data_layout
    }
}

impl<'ll, 'tcx> HasTargetSpec for CodegenCx<'ll, 'tcx> {
    fn target_spec(&self) -> &Target {
        &self.nvptx_target
    }
}

impl<'ll, 'tcx> ty::layout::HasTyCtxt<'tcx> for CodegenCx<'ll, 'tcx> {
    fn tcx(&self) -> TyCtxt<'tcx> {
        self.tcx
    }
}

impl<'ll, 'tcx> HasTypingEnv<'tcx> for CodegenCx<'ll, 'tcx> {
    fn typing_env(&self) -> TypingEnv<'tcx> {
        todo!()
    }
}

impl<'ll, 'tcx> LayoutOfHelpers<'tcx> for CodegenCx<'ll, 'tcx> {
    type LayoutOfResult = TyAndLayout<'tcx>;

    #[inline]
    fn handle_layout_err(&self, err: LayoutError<'tcx>, span: Span, ty: Ty<'tcx>) -> ! {
        if let LayoutError::SizeOverflow(_) = err {
            self.sess().span_fatal(span, &err.to_string())
        } else {
            span_bug!(span, "failed to get layout for `{}`: {}", ty, err)
        }
    }
}

impl<'tcx, 'll> HasParamEnv<'tcx> for CodegenCx<'ll, 'tcx> {
    fn param_env(&self) -> ty::ParamEnv<'tcx> {
        ty::ParamEnv::reveal_all()
    }
}

impl<'ll, 'tcx> FnAbiOfHelpers<'tcx> for CodegenCx<'ll, 'tcx> {
    type FnAbiOfResult = &'tcx FnAbi<'tcx, Ty<'tcx>>;

    #[inline]
    fn handle_fn_abi_err(
        &self,
        err: FnAbiError<'tcx>,
        span: Span,
        fn_abi_request: FnAbiRequest<'tcx>,
    ) -> ! {
        if let FnAbiError::Layout(LayoutError::SizeOverflow(_)) = err {
            self.tcx.sess.span_fatal(span, &err.to_string())
        } else {
            match fn_abi_request {
                FnAbiRequest::OfFnPtr { sig, extra_args } => {
                    span_bug!(
                        span,
                        "`fn_abi_of_fn_ptr({}, {:?})` failed: {}",
                        sig,
                        extra_args,
                        err
                    );
                }
                FnAbiRequest::OfInstance {
                    instance,
                    extra_args,
                } => {
                    span_bug!(
                        span,
                        "`fn_abi_of_instance({}, {:?})` failed: {}",
                        instance,
                        extra_args,
                        err
                    );
                }
            }
        }
    }
}

impl<'ll, 'tcx> CoverageInfoMethods<'tcx> for CodegenCx<'ll, 'tcx> {
    fn coverageinfo_finalize(&self) {
        todo!()
    }

    fn define_unused_fn(&self, _def_id: rustc_hir::def_id::DefId) {
        todo!()
    }

    fn get_pgo_func_name_var(
        &self,
        _instance: Instance<'tcx>,
    ) -> <CodegenCx<'ll, 'tcx> as rustc_codegen_ssa::traits::BackendTypes>::Value {
        todo!()
    }
}

impl<'ll, 'tcx> TypeMembershipCodegenMethods<'tcx> for CodegenCx<'ll, 'tcx> {}
