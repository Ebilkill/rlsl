use self::ty::layout::Layout;
use rustc_const_math::{ConstFloat, ConstInt};
use rspirv;
use std::collections::HashMap;
use rustc;
use rustc::hir;
use spirv;
use rustc::ty;
use rspirv::mr::Builder;
use syntax;
use rustc::mir;
use rustc::ty::{subst, TyCtxt};


use {Intrinsic, IntrinsicType, SpirvConstVal, SpirvFn, SpirvFunctionCall, SpirvTy, SpirvValue};
use self::hir::def_id::DefId;
pub struct SpirvCtx<'a, 'tcx: 'a> {
    pub tcx: ty::TyCtxt<'a, 'tcx, 'tcx>,
    pub builder: Builder,
    pub ty_cache: HashMap<ty::Ty<'tcx>, SpirvTy>,
    pub const_cache: HashMap<SpirvConstVal, SpirvValue>,
    pub forward_fns: HashMap<hir::def_id::DefId, SpirvFn>,
    pub intrinsic_fns: HashMap<hir::def_id::DefId, Intrinsic>,
    pub debug_symbols: bool,
    pub glsl_ext_id: spirv::Word,
}

impl<'a, 'tcx> SpirvCtx<'a, 'tcx> {
    /// Tries to get a function id, if it fails it looks for an intrinsic id
    pub fn get_function_call(&self, def_id: DefId) -> Option<SpirvFunctionCall> {
        self.forward_fns
            .get(&def_id)
            .map(|&spirv_fn| SpirvFunctionCall::Function(spirv_fn))
            .or(
                self.intrinsic_fns
                    .get(&def_id)
                    .map(|&id| SpirvFunctionCall::Intrinsic(id)),
            )
    }
    pub fn constant_f32(&mut self, value: f32, mtx: MirContext<'a, 'tcx>) -> SpirvValue {
        use std::convert::TryFrom;
        let val = SpirvConstVal::Float(ConstFloat::from_u128(
            TryFrom::try_from(value.to_bits()).expect("Could not convert from f32 to u128"),
            syntax::ast::FloatTy::F32,
        ));
        self.constant(val, mtx)
    }

    pub fn constant_u32(&mut self, value: u32, mtx: MirContext<'a, 'tcx>) -> SpirvValue {
        let val = SpirvConstVal::Integer(ConstInt::U32(value));
        self.constant(val, mtx)
    }

    pub fn constant(&mut self, val: SpirvConstVal, mtx: MirContext<'a, 'tcx>) -> SpirvValue {
        if let Some(val) = self.const_cache.get(&val) {
            return *val;
        }
        let spirv_val = match val {
            SpirvConstVal::Integer(const_int) => {
                use rustc::ty::util::IntTypeExt;
                let ty = const_int.int_type().to_ty(self.tcx);
                let spirv_ty = self.to_ty(ty, mtx, spirv::StorageClass::Function);
                let value = const_int.to_u128_unchecked() as u32;
                self.builder.constant_u32(spirv_ty.word, value)
            }
            SpirvConstVal::Float(const_float) => {
                use rustc::infer::unify_key::ToType;
                let value = const_float.to_i128(32).expect("Only f32 is supported") as f32;
                let ty = const_float.ty.to_type(self.tcx);
                let spirv_ty = self.to_ty(ty, mtx, spirv::StorageClass::Function);
                self.builder.constant_f32(spirv_ty.word, value)
            }
        };
        let spirv_expr = SpirvValue(spirv_val);
        self.const_cache.insert(val, spirv_expr);
        spirv_expr
    }
    pub fn to_ty(
        &mut self,
        ty: rustc::ty::Ty<'tcx>,
        mtx: MirContext<'a, 'tcx>,
        storage_class: spirv::StorageClass,
    ) -> SpirvTy {
        use rustc::ty::TypeVariants;
        let ty = mtx.monomorphize(&ty);
        let ty = match ty.sty {
            TypeVariants::TyRef(_, type_and_mut) => {
                let t = ty::TypeAndMut {
                    ty: type_and_mut.ty,
                    mutbl: rustc::hir::Mutability::MutMutable,
                };
                self.tcx.mk_ptr(t)
            }
            _ => ty,
        };
        if let Some(ty) = self.ty_cache.get(ty) {
            return *ty;
        }
        let spirv_type: SpirvTy = match ty.sty {
            // TODO: Proper TyNever
            TypeVariants::TyNever => 0.into(),
            TypeVariants::TyBool => self.builder.type_bool().into(),
            TypeVariants::TyInt(int_ty) => {
                self.builder
                    .type_int(int_ty.bit_width().unwrap_or(32) as u32, 1)
                    .into()
            }
            TypeVariants::TyUint(uint_ty) => {
                self.builder
                    .type_int(uint_ty.bit_width().unwrap_or(32) as u32, 0)
                    .into()
            }
            TypeVariants::TyFloat(f_ty) => {
                use syntax::ast::FloatTy;
                match f_ty {
                    FloatTy::F32 => self.builder.type_float(32).into(),
                    FloatTy::F64 => panic!("f64 is not supported"),
                }
            }
            TypeVariants::TyTuple(slice, _) if slice.len() == 0 => self.builder.type_void().into(),
            TypeVariants::TyTuple(slice, _) => {
                let field_ty_spirv: Vec<_> = slice
                    .iter()
                    .map(|ty| self.to_ty(ty, mtx, storage_class).word)
                    .collect();

                let spirv_struct = self.builder.type_struct(&field_ty_spirv);
                spirv_struct.into()
            }
            TypeVariants::TyFnPtr(sig) => {
                let ty = self.tcx
                    .erase_late_bound_regions_and_normalize(&sig.output());
                let ret_ty = self.to_ty(ty, mtx, storage_class);
                let input_ty: Vec<_> = sig.inputs()
                    .skip_binder()
                    .iter()
                    .map(|ty| self.to_ty(ty, mtx, storage_class).word)
                    .collect();
                self.builder.type_function(ret_ty.word, &input_ty).into()
            }
            TypeVariants::TyRawPtr(type_and_mut) => {
                let inner = self.to_ty(type_and_mut.ty, mtx, storage_class);
                self.builder
                    .type_pointer(None, spirv::StorageClass::Function, inner.word)
                    .into()
            }
            TypeVariants::TyParam(_) => panic!("TyParam should have been monomorphized"),
            TypeVariants::TyAdt(adt, substs) => {
                let mono_substs = mtx.monomorphize(&substs);
                match adt.adt_kind() {
                    ty::AdtKind::Enum => {
                        let layout =
                            ty.layout(self.tcx, ty::ParamEnv::empty(rustc::traits::Reveal::All))
                                .expect("layout");
                        let discr_ty = if let &Layout::General { discr, .. } = layout {
                            discr.to_ty(&self.tcx, false)
                        } else {
                            panic!("No enum layout")
                        };
                        let discr_ty_spirv = self.to_ty(discr_ty, mtx, storage_class);
                        let mut field_ty_spirv: Vec<_> = adt.variants
                            .iter()
                            .map(|variant| {
                                let variant_field_ty: Vec<_> = variant
                                    .fields
                                    .iter()
                                    .map(|field| {
                                        let ty = field.ty(self.tcx, mono_substs);
                                        self.to_ty(ty, mtx, storage_class).word
                                    })
                                    .collect();
                                let spirv_struct = self.builder.type_struct(&variant_field_ty);
                                if self.debug_symbols {
                                    for (index, field) in variant.fields.iter().enumerate() {
                                        self.builder.member_name(
                                            spirv_struct,
                                            index as u32,
                                            field.name.as_str().to_string(),
                                        );
                                    }
                                    self.name_from_def_id(variant.did, spirv_struct);
                                }
                                spirv_struct
                            })
                            .collect();
                        field_ty_spirv.push(discr_ty_spirv.word);

                        let spirv_struct = self.builder.type_struct(&field_ty_spirv);
                        if self.debug_symbols {
                            for (index, field) in adt.all_fields().enumerate() {
                                self.builder.member_name(
                                    spirv_struct,
                                    index as u32,
                                    field.name.as_str().to_string(),
                                );
                            }
                            self.name_from_def_id(adt.did, spirv_struct);
                        }
                        spirv_struct.into()
                    }
                    ty::AdtKind::Struct => {
                        let attrs = self.tcx.get_attrs(adt.did);
                        use std::ops::Deref;
                        let intrinsic = IntrinsicType::from_attr(attrs.deref());

                        if let Some(intrinsic) = intrinsic {
                            let intrinsic_spirv = match intrinsic {
                                IntrinsicType::Vec(dim) => {
                                    let field_ty = adt.all_fields()
                                        .nth(0)
                                        .map(|f| f.ty(self.tcx, mono_substs))
                                        .expect("no field");
                                    let spirv_ty = self.to_ty(field_ty, mtx, storage_class);
                                    self.builder.type_vector(spirv_ty.word, dim as u32).into()
                                }
                            };
                            intrinsic_spirv
                        } else {
                            let field_ty_spirv: Vec<_> = adt.all_fields()
                                .map(|f| {
                                    let ty = f.ty(self.tcx, mono_substs);
                                    self.to_ty(ty, mtx, storage_class).word
                                })
                                .collect();

                            let spirv_struct = self.builder.type_struct(&field_ty_spirv);
                            if self.debug_symbols {
                                for (index, field) in adt.all_fields().enumerate() {
                                    self.builder.member_name(
                                        spirv_struct,
                                        index as u32,
                                        field.name.as_str().to_string(),
                                    );
                                }
                            }
                            self.name_from_def_id(adt.did, spirv_struct);
                            spirv_struct.into()
                        }
                    }
                    ref r => unimplemented!("{:?}", r),
                }
            }
            ref r => unimplemented!("{:?}", r),
        };
        self.ty_cache.insert(ty, spirv_type);
        spirv_type
    }

    pub fn to_ty_as_ptr<'gcx>(
        &mut self,
        ty: ty::Ty<'tcx>,
        mtx: MirContext<'a, 'tcx>,
        storage_class: spirv::StorageClass,
    ) -> SpirvTy {
        let t = ty::TypeAndMut {
            ty,
            mutbl: rustc::hir::Mutability::MutMutable,
        };
        let ty_ptr = self.tcx.mk_ptr(t);
        self.to_ty(ty_ptr, mtx, storage_class)
    }
    fn attrs_from_def_id(&self, def_id: DefId) -> Option<&[syntax::ast::Attribute]> {
        let node_id = self.tcx.hir.as_local_node_id(def_id);
        let node = node_id.and_then(|id| self.tcx.hir.find(id));
        let item = node.and_then(|node| {
            match node {
                hir::map::Node::NodeItem(item) => Some(item),
                _ => None,
            }
        });
        item.map(|item| &*item.attrs)
    }
    pub fn name_from_def_id(&mut self, def_id: hir::def_id::DefId, id: spirv::Word) {
        let name = self.tcx
            .hir
            .as_local_node_id(def_id)
            .map(|node_id| self.tcx.hir.name(node_id).as_str());
        if let Some(name) = name {
            if self.debug_symbols {
                self.builder.name(id, name.as_ref());
            }
        }
    }
    pub fn name_from_str(&mut self, name: &str, id: spirv::Word) {
        if self.debug_symbols {
            self.builder.name(id, name);
        }
    }
    pub fn build_module(self) {
        use rspirv::binary::Assemble;
        use std::mem::size_of;
        use std::fs::File;
        use std::io::Write;
        let mut f = File::create("shader.spv").unwrap();
        let spirv_module = self.builder.module();
        let bytes: Vec<u8> = spirv_module
            .assemble()
            .iter()
            .flat_map(|val| {
                (0..size_of::<u32>()).map(move |i| ((val >> (8 * i)) & 0xff) as u8)
            })
            .collect();
        let mut loader = rspirv::mr::Loader::new();
        //let bytes = b.module().assemble_bytes();
        rspirv::binary::parse_bytes(&bytes, &mut loader).expect("parse bytes");
        f.write_all(&bytes).expect("write bytes");
    }
    pub fn new(tcx: ty::TyCtxt<'a, 'tcx, 'tcx>) -> SpirvCtx<'a, 'tcx> {
        let mut builder = Builder::new();
        builder.capability(spirv::Capability::Shader);
        let glsl_ext_id = builder.ext_inst_import("GLSL.std.450");
        builder.memory_model(spirv::AddressingModel::Logical, spirv::MemoryModel::GLSL450);
        SpirvCtx {
            debug_symbols: true,
            builder,
            ty_cache: HashMap::new(),
            const_cache: HashMap::new(),
            forward_fns: HashMap::new(),
            intrinsic_fns: HashMap::new(),
            tcx,
            glsl_ext_id,
        }
    }
}

#[derive(Copy, Clone)]
pub struct MirContext<'a, 'tcx: 'a> {
    pub def_id: hir::def_id::DefId,
    pub tcx: TyCtxt<'a, 'tcx, 'tcx>,
    pub mir: &'a mir::Mir<'tcx>,
    pub substs: &'tcx subst::Substs<'tcx>,
}
impl<'a, 'tcx> MirContext<'a, 'tcx> {
    pub fn monomorphize<T>(&self, value: &T) -> T
    where
        T: rustc::infer::TransNormalize<'tcx>,
    {
        self.tcx.trans_apply_param_substs(self.substs, value)
    }
}