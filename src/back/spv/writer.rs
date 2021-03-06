/*! Standard Portable Intermediate Representation (SPIR-V) backend !*/
use super::{Instruction, LogicalLayout, Options, PhysicalLayout, WriterFlags};
use crate::{
    arena::{Arena, Handle},
    proc::{
        analyzer::{Analysis, FunctionInfo},
        Layouter, ResolveContext, Typifier, TypifyError,
    },
};
use spirv::Word;
use std::{collections::hash_map::Entry, ops};
use thiserror::Error;

const BITS_PER_BYTE: crate::Bytes = 8;

#[derive(Clone, Debug, Error)]
pub enum Error {
    #[error("target SPIRV-{0}.{1} is not supported")]
    UnsupportedVersion(u8, u8),
    #[error("one of the required capabilities {0:?} is missing")]
    MissingCapabilities(Vec<spirv::Capability>),
    #[error("unimplemented {0}")]
    FeatureNotImplemented(&'static str),
    #[error(transparent)]
    Resolve(#[from] TypifyError),
}

struct Block {
    label_id: Word,
    body: Vec<Instruction>,
    termination: Option<Instruction>,
}

impl Block {
    fn new(label_id: Word) -> Self {
        Block {
            label_id,
            body: Vec::new(),
            termination: None,
        }
    }
}

struct LocalVariable {
    id: Word,
    instruction: Instruction,
}

#[derive(Default)]
struct Function {
    signature: Option<Instruction>,
    parameters: Vec<Instruction>,
    variables: crate::FastHashMap<Handle<crate::LocalVariable>, LocalVariable>,
    blocks: Vec<Block>,
}

impl Function {
    fn to_words(&self, sink: &mut impl Extend<Word>) {
        self.signature.as_ref().unwrap().to_words(sink);
        for instruction in self.parameters.iter() {
            instruction.to_words(sink);
        }
        for (index, block) in self.blocks.iter().enumerate() {
            Instruction::label(block.label_id).to_words(sink);
            if index == 0 {
                for local_var in self.variables.values() {
                    local_var.instruction.to_words(sink);
                }
            }
            for instruction in block.body.iter() {
                instruction.to_words(sink);
            }
            block.termination.as_ref().unwrap().to_words(sink);
        }
    }

    fn consume(&mut self, mut block: Block, termination: Instruction) {
        block.termination = Some(termination);
        self.blocks.push(block);
    }
}

#[derive(Debug, PartialEq, Hash, Eq, Copy, Clone)]
enum LocalType {
    Value {
        vector_size: Option<crate::VectorSize>,
        kind: crate::ScalarKind,
        width: crate::Bytes,
        pointer_class: Option<crate::StorageClass>,
    },
    Matrix {
        columns: crate::VectorSize,
        rows: crate::VectorSize,
        width: crate::Bytes,
    },
    Pointer {
        base: Handle<crate::Type>,
        class: crate::StorageClass,
    },
    SampledImage {
        image_type: Handle<crate::Type>,
    },
}

impl LocalType {
    fn from_inner(inner: &crate::TypeInner) -> Option<Self> {
        Some(match *inner {
            crate::TypeInner::Scalar { kind, width } => LocalType::Value {
                vector_size: None,
                kind,
                width,
                pointer_class: None,
            },
            crate::TypeInner::Vector { size, kind, width } => LocalType::Value {
                vector_size: Some(size),
                kind,
                width,
                pointer_class: None,
            },
            crate::TypeInner::Matrix {
                columns,
                rows,
                width,
            } => LocalType::Matrix {
                columns,
                rows,
                width,
            },
            crate::TypeInner::Pointer { base, class } => LocalType::Pointer { base, class },
            crate::TypeInner::ValuePointer {
                size,
                kind,
                width,
                class,
            } => LocalType::Value {
                vector_size: size,
                kind,
                width,
                pointer_class: Some(class),
            },
            _ => return None,
        })
    }
}

#[derive(Debug, PartialEq, Hash, Eq, Copy, Clone)]
enum LookupType {
    Handle(Handle<crate::Type>),
    Local(LocalType),
}

impl From<LocalType> for LookupType {
    fn from(local: LocalType) -> Self {
        Self::Local(local)
    }
}

fn map_dim(dim: crate::ImageDimension) -> spirv::Dim {
    match dim {
        crate::ImageDimension::D1 => spirv::Dim::Dim1D,
        crate::ImageDimension::D2 => spirv::Dim::Dim2D,
        crate::ImageDimension::D3 => spirv::Dim::Dim2D,
        crate::ImageDimension::Cube => spirv::Dim::DimCube,
    }
}

#[derive(Debug, PartialEq, Clone, Hash, Eq)]
struct LookupFunctionType {
    parameter_type_ids: Vec<Word>,
    return_type_id: Word,
}

#[derive(Debug)]
enum Dimension {
    Scalar,
    Vector,
    Matrix,
}

fn get_dimension(type_inner: &crate::TypeInner) -> Dimension {
    match *type_inner {
        crate::TypeInner::Scalar { .. } => Dimension::Scalar,
        crate::TypeInner::Vector { .. } => Dimension::Vector,
        crate::TypeInner::Matrix { .. } => Dimension::Matrix,
        _ => unreachable!(),
    }
}

#[derive(Clone, Copy, Default)]
struct LoopContext {
    continuing_id: Option<Word>,
    break_id: Option<Word>,
}

#[derive(Default)]
struct CachedExpressions {
    ids: Vec<Word>,
}
impl CachedExpressions {
    fn reset(&mut self, length: usize) {
        self.ids.clear();
        self.ids.resize(length, 0);
    }
}
impl ops::Index<Handle<crate::Expression>> for CachedExpressions {
    type Output = Word;
    fn index(&self, h: Handle<crate::Expression>) -> &Word {
        let id = &self.ids[h.index()];
        if *id == 0 {
            unreachable!("Expression {:?} is not cached!", h);
        }
        id
    }
}
impl ops::IndexMut<Handle<crate::Expression>> for CachedExpressions {
    fn index_mut(&mut self, h: Handle<crate::Expression>) -> &mut Word {
        let id = &mut self.ids[h.index()];
        if *id != 0 {
            unreachable!("Expression {:?} is already cached!", h);
        }
        id
    }
}

struct GlobalVariable {
    /// Actual ID of the variable.
    id: Word,
    /// For `StorageClass::Handle` variables, this ID is recorded in the function
    /// prelude block (and reset before every function) as `OpLoad` of the variable.
    /// It is then used for all the global ops, such as `OpImageSample`.
    handle_id: Word,
    /// SPIR-V storage class.
    class: spirv::StorageClass,
}

pub struct Writer {
    physical_layout: PhysicalLayout,
    logical_layout: LogicalLayout,
    id_count: u32,
    capabilities: crate::FastHashSet<spirv::Capability>,
    debugs: Vec<Instruction>,
    annotations: Vec<Instruction>,
    flags: WriterFlags,
    void_type: u32,
    //TODO: convert most of these into vectors, addressable by handle indices
    lookup_type: crate::FastHashMap<LookupType, Word>,
    lookup_function: crate::FastHashMap<Handle<crate::Function>, Word>,
    lookup_function_type: crate::FastHashMap<LookupFunctionType, Word>,
    lookup_function_call: crate::FastHashMap<Handle<crate::Expression>, Word>,
    lookup_constant: crate::FastHashMap<Handle<crate::Constant>, Word>,
    global_variables: Vec<GlobalVariable>,
    cached: CachedExpressions,
    // TODO: this is a type property that depends on the global variable that uses it
    // so it may require us to duplicate the type!
    struct_type_handles: crate::FastHashMap<Handle<crate::Type>, crate::StorageAccess>,
    gl450_ext_inst_id: Word,
    layouter: Layouter,
    typifier: Typifier,
    temp_chain: Vec<Word>,
}

impl Writer {
    pub fn new(options: &Options) -> Result<Self, Error> {
        let (major, minor) = options.lang_version;
        if major != 1 {
            return Err(Error::UnsupportedVersion(major, minor));
        }
        let raw_version = ((major as u32) << 16) | ((minor as u32) << 8);
        let gl450_ext_inst_id = 1;
        let void_type = 2;
        Ok(Writer {
            physical_layout: PhysicalLayout::new(raw_version),
            logical_layout: LogicalLayout::default(),
            id_count: 2, // see `gl450_ext_inst_id` and `void_type`
            capabilities: options.capabilities.clone(),
            debugs: vec![],
            annotations: vec![],
            flags: options.flags,
            void_type,
            lookup_type: crate::FastHashMap::default(),
            lookup_function: crate::FastHashMap::default(),
            lookup_function_type: crate::FastHashMap::default(),
            lookup_function_call: crate::FastHashMap::default(),
            lookup_constant: crate::FastHashMap::default(),
            global_variables: Vec::new(),
            cached: CachedExpressions::default(),
            struct_type_handles: crate::FastHashMap::default(),
            gl450_ext_inst_id,
            layouter: Layouter::default(),
            typifier: Typifier::new(),
            temp_chain: Vec::new(),
        })
    }

    fn generate_id(&mut self) -> Word {
        self.id_count += 1;
        self.id_count
    }

    fn check(&mut self, capabilities: &[spirv::Capability]) -> Result<(), Error> {
        if capabilities.is_empty()
            || capabilities
                .iter()
                .any(|cap| self.capabilities.contains(cap))
        {
            Ok(())
        } else {
            Err(Error::MissingCapabilities(capabilities.to_vec()))
        }
    }

    fn get_type_id(
        &mut self,
        arena: &Arena<crate::Type>,
        lookup_ty: LookupType,
    ) -> Result<Word, Error> {
        if let Entry::Occupied(e) = self.lookup_type.entry(lookup_ty) {
            Ok(*e.get())
        } else {
            match lookup_ty {
                LookupType::Handle(handle) => match LocalType::from_inner(&arena[handle].inner) {
                    Some(local) => self.get_type_id(arena, LookupType::Local(local)),
                    None => self.write_type_declaration_arena(arena, handle),
                },
                LookupType::Local(local_ty) => self.write_type_declaration_local(arena, local_ty),
            }
        }
    }

    fn get_pointer_id(
        &mut self,
        arena: &Arena<crate::Type>,
        handle: Handle<crate::Type>,
        class: crate::StorageClass,
    ) -> Result<Word, Error> {
        let ty_id = self.get_type_id(arena, LookupType::Handle(handle))?;
        if let crate::TypeInner::Pointer { .. } = arena[handle].inner {
            return Ok(ty_id);
        }
        Ok(
            match self
                .lookup_type
                .entry(LookupType::Local(LocalType::Pointer {
                    base: handle,
                    class,
                })) {
                Entry::Occupied(e) => *e.get(),
                _ => {
                    let storage_class = self.parse_to_spirv_storage_class(class);
                    let id = self.generate_id();
                    let instruction = Instruction::type_pointer(id, storage_class, ty_id);
                    instruction.to_words(&mut self.logical_layout.declarations);
                    self.lookup_type.insert(
                        LookupType::Local(LocalType::Pointer {
                            base: handle,
                            class,
                        }),
                        id,
                    );
                    id
                }
            },
        )
    }

    fn create_constant(&mut self, type_id: Word, value: &[Word]) -> Word {
        let id = self.generate_id();
        let instruction = Instruction::constant(type_id, id, value);
        instruction.to_words(&mut self.logical_layout.declarations);
        id
    }

    fn write_function(
        &mut self,
        ir_function: &crate::Function,
        info: &FunctionInfo,
        ir_module: &crate::Module,
    ) -> Result<Word, Error> {
        let mut function = Function::default();

        self.typifier.resolve_all(
            &ir_function.expressions,
            &ir_module.types,
            &ResolveContext {
                constants: &ir_module.constants,
                global_vars: &ir_module.global_variables,
                local_vars: &ir_function.local_variables,
                functions: &ir_module.functions,
                arguments: &ir_function.arguments,
            },
        )?;

        for (handle, variable) in ir_function.local_variables.iter() {
            let id = self.generate_id();

            if self.flags.contains(WriterFlags::DEBUG) {
                if let Some(ref name) = variable.name {
                    self.debugs.push(Instruction::name(id, name));
                }
            }

            let init_word = variable
                .init
                .map(|constant| self.lookup_constant[&constant]);
            let pointer_type_id =
                self.get_pointer_id(&ir_module.types, variable.ty, crate::StorageClass::Function)?;
            let instruction = Instruction::variable(
                pointer_type_id,
                id,
                spirv::StorageClass::Function,
                init_word,
            );
            function
                .variables
                .insert(handle, LocalVariable { id, instruction });
        }

        let return_type_id = match ir_function.return_type {
            Some(handle) => self.get_type_id(&ir_module.types, LookupType::Handle(handle))?,
            None => self.void_type,
        };
        let mut parameter_type_ids = Vec::with_capacity(ir_function.arguments.len());

        for argument in ir_function.arguments.iter() {
            let id = self.generate_id();
            let parameter_type_id =
                self.get_type_id(&ir_module.types, LookupType::Handle(argument.ty))?;
            parameter_type_ids.push(parameter_type_id);
            function
                .parameters
                .push(Instruction::function_parameter(parameter_type_id, id));
        }

        let lookup_function_type = LookupFunctionType {
            return_type_id,
            parameter_type_ids,
        };

        let function_id = self.generate_id();
        if self.flags.contains(WriterFlags::DEBUG) {
            if let Some(ref name) = ir_function.name {
                self.debugs.push(Instruction::name(function_id, name));
            }
        }

        let function_type = self.get_function_type(lookup_function_type);
        function.signature = Some(Instruction::function(
            return_type_id,
            function_id,
            spirv::FunctionControl::empty(),
            function_type,
        ));

        let prelude_id = self.generate_id();
        let mut prelude = Block::new(prelude_id);

        // fill up the `GlobalVariable::handle_id`
        for gv in self.global_variables.iter_mut() {
            gv.handle_id = 0;
        }
        for (handle, var) in ir_module.global_variables.iter() {
            // Handle globals are pre-emitted and should be loaded automatically.
            if info[handle].is_empty() || var.class != crate::StorageClass::Handle {
                continue;
            }
            let id = self.generate_id();
            let result_type_id = self.get_type_id(&ir_module.types, LookupType::Handle(var.ty))?;
            let gv = &mut self.global_variables[handle.index()];
            prelude
                .body
                .push(Instruction::load(result_type_id, id, gv.id, None));
            gv.handle_id = id;
        }
        // fill up the pre-emitted expressions
        self.cached.reset(ir_function.expressions.len());
        for (handle, expr) in ir_function.expressions.iter() {
            if expr.needs_pre_emit() {
                self.cache_expression_value(
                    ir_module,
                    ir_function,
                    handle,
                    &mut prelude,
                    &mut function,
                )?;
            }
        }

        let main_id = self.generate_id();
        function.consume(prelude, Instruction::branch(main_id));
        self.write_block(
            main_id,
            &ir_function.body,
            ir_module,
            ir_function,
            &mut function,
            None,
            LoopContext::default(),
        )?;

        function.to_words(&mut self.logical_layout.function_definitions);
        Instruction::function_end().to_words(&mut self.logical_layout.function_definitions);

        Ok(function_id)
    }

    // TODO Move to instructions module
    fn write_entry_point(
        &mut self,
        entry_point: &crate::EntryPoint,
        info: &FunctionInfo,
        ir_module: &crate::Module,
    ) -> Result<Instruction, Error> {
        let function_id = self.write_function(&entry_point.function, info, ir_module)?;

        let mut interface_ids = Vec::new();
        for (handle, var) in ir_module.global_variables.iter() {
            if info[handle].is_empty() {
                continue;
            }
            if let crate::StorageClass::Input | crate::StorageClass::Output = var.class {
                let id = self.global_variables[handle.index()].id;
                interface_ids.push(id);
            }
        }

        let exec_model = match entry_point.stage {
            crate::ShaderStage::Vertex => spirv::ExecutionModel::Vertex,
            crate::ShaderStage::Fragment => {
                let execution_mode = spirv::ExecutionMode::OriginUpperLeft;
                self.check(execution_mode.required_capabilities())?;
                Instruction::execution_mode(function_id, execution_mode, &[])
                    .to_words(&mut self.logical_layout.execution_modes);
                spirv::ExecutionModel::Fragment
            }
            crate::ShaderStage::Compute => {
                let execution_mode = spirv::ExecutionMode::LocalSize;
                self.check(execution_mode.required_capabilities())?;
                Instruction::execution_mode(
                    function_id,
                    execution_mode,
                    &entry_point.workgroup_size,
                )
                .to_words(&mut self.logical_layout.execution_modes);
                spirv::ExecutionModel::GLCompute
            }
        };
        self.check(exec_model.required_capabilities())?;

        if self.flags.contains(WriterFlags::DEBUG) {
            self.debugs
                .push(Instruction::name(function_id, &entry_point.name));
        }

        Ok(Instruction::entry_point(
            exec_model,
            function_id,
            &entry_point.name,
            interface_ids.as_slice(),
        ))
    }

    fn write_scalar(&self, id: Word, kind: crate::ScalarKind, width: crate::Bytes) -> Instruction {
        let bits = (width * BITS_PER_BYTE) as u32;
        match kind {
            crate::ScalarKind::Sint => {
                Instruction::type_int(id, bits, super::instructions::Signedness::Signed)
            }
            crate::ScalarKind::Uint => {
                Instruction::type_int(id, bits, super::instructions::Signedness::Unsigned)
            }
            crate::ScalarKind::Float => Instruction::type_float(id, bits),
            crate::ScalarKind::Bool => Instruction::type_bool(id),
        }
    }

    fn parse_to_spirv_storage_class(&self, class: crate::StorageClass) -> spirv::StorageClass {
        match class {
            crate::StorageClass::Handle => spirv::StorageClass::UniformConstant,
            crate::StorageClass::Function => spirv::StorageClass::Function,
            crate::StorageClass::Input => spirv::StorageClass::Input,
            crate::StorageClass::Output => spirv::StorageClass::Output,
            crate::StorageClass::Private => spirv::StorageClass::Private,
            crate::StorageClass::Storage if self.physical_layout.supports_storage_buffers() => {
                spirv::StorageClass::StorageBuffer
            }
            crate::StorageClass::Storage | crate::StorageClass::Uniform => {
                spirv::StorageClass::Uniform
            }
            crate::StorageClass::WorkGroup => spirv::StorageClass::Workgroup,
            crate::StorageClass::PushConstant => spirv::StorageClass::PushConstant,
        }
    }

    fn write_type_declaration_local(
        &mut self,
        arena: &Arena<crate::Type>,
        local_ty: LocalType,
    ) -> Result<Word, Error> {
        let id = self.generate_id();
        let instruction = match local_ty {
            LocalType::Value {
                vector_size: None,
                kind,
                width,
                pointer_class: None,
            } => self.write_scalar(id, kind, width),
            LocalType::Value {
                vector_size: Some(size),
                kind,
                width,
                pointer_class: None,
            } => {
                let scalar_id = self.get_type_id(
                    arena,
                    LookupType::Local(LocalType::Value {
                        vector_size: None,
                        kind,
                        width,
                        pointer_class: None,
                    }),
                )?;
                Instruction::type_vector(id, scalar_id, size)
            }
            LocalType::Matrix {
                columns,
                rows,
                width,
            } => {
                let vector_id = self.get_type_id(
                    arena,
                    LookupType::Local(LocalType::Value {
                        vector_size: Some(rows),
                        kind: crate::ScalarKind::Float,
                        width,
                        pointer_class: None,
                    }),
                )?;
                Instruction::type_matrix(id, vector_id, columns)
            }
            LocalType::Pointer { base, class } => {
                let type_id = self.get_type_id(arena, LookupType::Handle(base))?;
                Instruction::type_pointer(id, self.parse_to_spirv_storage_class(class), type_id)
            }
            LocalType::Value {
                vector_size,
                kind,
                width,
                pointer_class: Some(class),
            } => {
                let type_id = self.get_type_id(
                    arena,
                    LookupType::Local(LocalType::Value {
                        vector_size,
                        kind,
                        width,
                        pointer_class: None,
                    }),
                )?;
                Instruction::type_pointer(id, self.parse_to_spirv_storage_class(class), type_id)
            }
            LocalType::SampledImage { image_type } => {
                let image_type_id = self.get_type_id(arena, LookupType::Handle(image_type))?;
                Instruction::type_sampled_image(id, image_type_id)
            }
        };

        self.lookup_type.insert(LookupType::Local(local_ty), id);
        instruction.to_words(&mut self.logical_layout.declarations);
        Ok(id)
    }

    fn write_type_declaration_arena(
        &mut self,
        arena: &Arena<crate::Type>,
        handle: Handle<crate::Type>,
    ) -> Result<Word, Error> {
        let ty = &arena[handle];
        let id = self.generate_id();

        if self.flags.contains(WriterFlags::DEBUG) {
            if let Some(ref name) = ty.name {
                self.debugs.push(Instruction::name(id, name));
            }
        }

        let instruction = match ty.inner {
            crate::TypeInner::Scalar { kind, width } => {
                self.lookup_type.insert(
                    LookupType::Local(LocalType::Value {
                        vector_size: None,
                        kind,
                        width,
                        pointer_class: None,
                    }),
                    id,
                );
                self.write_scalar(id, kind, width)
            }
            crate::TypeInner::Vector { size, kind, width } => {
                let scalar_id = self.get_type_id(
                    arena,
                    LookupType::Local(LocalType::Value {
                        vector_size: None,
                        kind,
                        width,
                        pointer_class: None,
                    }),
                )?;
                self.lookup_type.insert(
                    LookupType::Local(LocalType::Value {
                        vector_size: Some(size),
                        kind,
                        width,
                        pointer_class: None,
                    }),
                    id,
                );
                Instruction::type_vector(id, scalar_id, size)
            }
            crate::TypeInner::Matrix {
                columns,
                rows,
                width,
            } => {
                let vector_id = self.get_type_id(
                    arena,
                    LookupType::Local(LocalType::Value {
                        vector_size: Some(columns),
                        kind: crate::ScalarKind::Float,
                        width,
                        pointer_class: None,
                    }),
                )?;
                self.lookup_type.insert(
                    LookupType::Local(LocalType::Matrix {
                        columns,
                        rows,
                        width,
                    }),
                    id,
                );
                Instruction::type_matrix(id, vector_id, columns)
            }
            crate::TypeInner::Image {
                dim,
                arrayed,
                class,
            } => {
                let kind = match class {
                    crate::ImageClass::Sampled { kind, multi: _ } => kind,
                    crate::ImageClass::Depth => crate::ScalarKind::Float,
                    crate::ImageClass::Storage(format) => format.into(),
                };
                let local_type = LocalType::Value {
                    vector_size: None,
                    kind,
                    width: 4,
                    pointer_class: None,
                };
                let type_id = self.get_type_id(arena, LookupType::Local(local_type))?;
                let dim = map_dim(dim);
                self.check(dim.required_capabilities())?;
                Instruction::type_image(id, type_id, dim, arrayed, class)
            }
            crate::TypeInner::Sampler { comparison: _ } => Instruction::type_sampler(id),
            crate::TypeInner::Array { base, size, stride } => {
                if let Some(array_stride) = stride {
                    self.annotations.push(Instruction::decorate(
                        id,
                        spirv::Decoration::ArrayStride,
                        &[array_stride.get()],
                    ));
                }

                let type_id = self.get_type_id(arena, LookupType::Handle(base))?;
                match size {
                    crate::ArraySize::Constant(const_handle) => {
                        let length_id = self.lookup_constant[&const_handle];
                        Instruction::type_array(id, type_id, length_id)
                    }
                    crate::ArraySize::Dynamic => Instruction::type_runtime_array(id, type_id),
                }
            }
            crate::TypeInner::Struct {
                block: true,
                ref members,
            } => {
                if let Some(&access) = self.struct_type_handles.get(&handle) {
                    let decoration = if access.is_empty() {
                        spirv::Decoration::Block
                    } else {
                        spirv::Decoration::BufferBlock
                    };
                    self.annotations
                        .push(Instruction::decorate(id, decoration, &[]));
                }

                let mut current_offset = 0;
                let mut member_ids = Vec::with_capacity(members.len());
                for (index, member) in members.iter().enumerate() {
                    let layout = self.layouter.resolve(member.ty);
                    current_offset += layout.pad(current_offset);
                    self.annotations.push(Instruction::member_decorate(
                        id,
                        index as u32,
                        spirv::Decoration::Offset,
                        &[current_offset],
                    ));
                    current_offset += match member.span {
                        Some(span) => span.get(),
                        None => layout.size,
                    };

                    if self.flags.contains(WriterFlags::DEBUG) {
                        if let Some(ref name) = member.name {
                            self.debugs
                                .push(Instruction::member_name(id, index as u32, name));
                        }
                    }

                    if let crate::TypeInner::Matrix {
                        columns,
                        rows: _,
                        width,
                    } = arena[member.ty].inner
                    {
                        let byte_stride = match columns {
                            crate::VectorSize::Bi => 2 * width,
                            crate::VectorSize::Tri | crate::VectorSize::Quad => 4 * width,
                        };
                        self.annotations.push(Instruction::member_decorate(
                            id,
                            index as u32,
                            spirv::Decoration::ColMajor,
                            &[],
                        ));
                        self.annotations.push(Instruction::member_decorate(
                            id,
                            index as u32,
                            spirv::Decoration::MatrixStride,
                            &[byte_stride as u32],
                        ));
                    }

                    let member_id = self.get_type_id(arena, LookupType::Handle(member.ty))?;
                    member_ids.push(member_id);
                }
                Instruction::type_struct(id, member_ids.as_slice())
            }
            crate::TypeInner::Struct {
                block: false,
                ref members,
            } => {
                let mut member_ids = Vec::with_capacity(members.len());
                for member in members {
                    let member_id = self.get_type_id(arena, LookupType::Handle(member.ty))?;
                    member_ids.push(member_id);
                }
                Instruction::type_struct(id, member_ids.as_slice())
            }
            crate::TypeInner::Pointer { base, class } => {
                let type_id = self.get_type_id(arena, LookupType::Handle(base))?;
                self.lookup_type
                    .insert(LookupType::Local(LocalType::Pointer { base, class }), id);
                Instruction::type_pointer(id, self.parse_to_spirv_storage_class(class), type_id)
            }
            crate::TypeInner::ValuePointer {
                size,
                kind,
                width,
                class,
            } => {
                let type_id = self.get_type_id(
                    arena,
                    LookupType::Local(LocalType::Value {
                        vector_size: size,
                        kind,
                        width,
                        pointer_class: None,
                    }),
                )?;
                self.lookup_type.insert(
                    LookupType::Local(LocalType::Value {
                        vector_size: size,
                        kind,
                        width,
                        pointer_class: Some(class),
                    }),
                    id,
                );
                Instruction::type_pointer(id, self.parse_to_spirv_storage_class(class), type_id)
            }
        };

        self.lookup_type.insert(LookupType::Handle(handle), id);
        instruction.to_words(&mut self.logical_layout.declarations);
        Ok(id)
    }

    fn write_constant_type(
        &mut self,
        id: Word,
        inner: &crate::ConstantInner,
        types: &Arena<crate::Type>,
    ) -> Result<(), Error> {
        let instruction = match *inner {
            crate::ConstantInner::Scalar { width, ref value } => {
                let type_id = self.get_type_id(
                    types,
                    LookupType::Local(LocalType::Value {
                        vector_size: None,
                        kind: value.scalar_kind(),
                        width,
                        pointer_class: None,
                    }),
                )?;
                let (solo, pair);
                match *value {
                    crate::ScalarValue::Sint(val) => {
                        let words = match width {
                            4 => {
                                solo = [val as u32];
                                &solo[..]
                            }
                            8 => {
                                pair = [(val >> 32) as u32, val as u32];
                                &pair
                            }
                            _ => unreachable!(),
                        };
                        Instruction::constant(type_id, id, words)
                    }
                    crate::ScalarValue::Uint(val) => {
                        let words = match width {
                            4 => {
                                solo = [val as u32];
                                &solo[..]
                            }
                            8 => {
                                pair = [(val >> 32) as u32, val as u32];
                                &pair
                            }
                            _ => unreachable!(),
                        };
                        Instruction::constant(type_id, id, words)
                    }
                    crate::ScalarValue::Float(val) => {
                        let words = match width {
                            4 => {
                                solo = [(val as f32).to_bits()];
                                &solo[..]
                            }
                            8 => {
                                let bits = f64::to_bits(val);
                                pair = [(bits >> 32) as u32, bits as u32];
                                &pair
                            }
                            _ => unreachable!(),
                        };
                        Instruction::constant(type_id, id, words)
                    }
                    crate::ScalarValue::Bool(true) => Instruction::constant_true(type_id, id),
                    crate::ScalarValue::Bool(false) => Instruction::constant_false(type_id, id),
                }
            }
            crate::ConstantInner::Composite { ty, ref components } => {
                let mut constituent_ids = Vec::with_capacity(components.len());
                for constituent in components.iter() {
                    let constituent_id = self.lookup_constant[constituent];
                    constituent_ids.push(constituent_id);
                }

                let type_id = self.get_type_id(types, LookupType::Handle(ty))?;
                Instruction::constant_composite(type_id, id, constituent_ids.as_slice())
            }
        };

        instruction.to_words(&mut self.logical_layout.declarations);
        Ok(())
    }

    fn write_global_variable(
        &mut self,
        ir_module: &crate::Module,
        handle: Handle<crate::GlobalVariable>,
    ) -> Result<(Instruction, Word, spirv::StorageClass), Error> {
        let global_variable = &ir_module.global_variables[handle];
        let id = self.generate_id();

        let class = self.parse_to_spirv_storage_class(global_variable.class);
        self.check(class.required_capabilities())?;

        let init_word = global_variable
            .init
            .map(|constant| self.lookup_constant[&constant]);
        let pointer_type_id =
            self.get_pointer_id(&ir_module.types, global_variable.ty, global_variable.class)?;
        let instruction = Instruction::variable(pointer_type_id, id, class, init_word);

        if self.flags.contains(WriterFlags::DEBUG) {
            if let Some(ref name) = global_variable.name {
                self.debugs.push(Instruction::name(id, name));
            }
        }

        let access_decoration = match global_variable.storage_access {
            crate::StorageAccess::LOAD => Some(spirv::Decoration::NonWritable),
            crate::StorageAccess::STORE => Some(spirv::Decoration::NonReadable),
            _ => None,
        };
        if let Some(decoration) = access_decoration {
            self.annotations
                .push(Instruction::decorate(id, decoration, &[]));
        }

        if let Some(interpolation) = global_variable.interpolation {
            let decoration = match interpolation {
                crate::Interpolation::Linear => Some(spirv::Decoration::NoPerspective),
                crate::Interpolation::Flat => Some(spirv::Decoration::Flat),
                crate::Interpolation::Patch => Some(spirv::Decoration::Patch),
                crate::Interpolation::Centroid => Some(spirv::Decoration::Centroid),
                crate::Interpolation::Sample => Some(spirv::Decoration::Sample),
                crate::Interpolation::Perspective => None,
            };
            if let Some(decoration) = decoration {
                self.annotations
                    .push(Instruction::decorate(id, decoration, &[]));
            }
        }

        match global_variable.binding {
            Some(crate::Binding::Location(location)) => {
                self.annotations.push(Instruction::decorate(
                    id,
                    spirv::Decoration::Location,
                    &[location],
                ));
            }
            Some(crate::Binding::Resource { group, binding }) => {
                self.annotations.push(Instruction::decorate(
                    id,
                    spirv::Decoration::DescriptorSet,
                    &[group],
                ));
                self.annotations.push(Instruction::decorate(
                    id,
                    spirv::Decoration::Binding,
                    &[binding],
                ));
            }
            Some(crate::Binding::BuiltIn(built_in)) => {
                use crate::BuiltIn as Bi;
                let built_in = match built_in {
                    Bi::BaseInstance => spirv::BuiltIn::BaseInstance,
                    Bi::BaseVertex => spirv::BuiltIn::BaseVertex,
                    Bi::ClipDistance => spirv::BuiltIn::ClipDistance,
                    Bi::InstanceIndex => spirv::BuiltIn::InstanceIndex,
                    Bi::PointSize => spirv::BuiltIn::PointSize,
                    Bi::Position => spirv::BuiltIn::Position,
                    Bi::VertexIndex => spirv::BuiltIn::VertexIndex,
                    // fragment
                    Bi::FragCoord => spirv::BuiltIn::FragCoord,
                    Bi::FragDepth => spirv::BuiltIn::FragDepth,
                    Bi::FrontFacing => spirv::BuiltIn::FrontFacing,
                    Bi::SampleIndex => spirv::BuiltIn::SampleId,
                    Bi::SampleMaskIn => spirv::BuiltIn::SampleMask,
                    Bi::SampleMaskOut => spirv::BuiltIn::SampleMask,
                    // compute
                    Bi::GlobalInvocationId => spirv::BuiltIn::GlobalInvocationId,
                    Bi::LocalInvocationId => spirv::BuiltIn::LocalInvocationId,
                    Bi::LocalInvocationIndex => spirv::BuiltIn::LocalInvocationIndex,
                    Bi::WorkGroupId => spirv::BuiltIn::WorkgroupId,
                    Bi::WorkGroupSize => spirv::BuiltIn::WorkgroupSize,
                };

                self.annotations.push(Instruction::decorate(
                    id,
                    spirv::Decoration::BuiltIn,
                    &[built_in as u32],
                ));
            }
            None => {}
        }

        // TODO Initializer is optional and not (yet) included in the IR
        Ok((instruction, id, class))
    }

    fn get_function_type(&mut self, lookup_function_type: LookupFunctionType) -> Word {
        match self
            .lookup_function_type
            .entry(lookup_function_type.clone())
        {
            Entry::Occupied(e) => *e.get(),
            _ => {
                let id = self.generate_id();
                let instruction = Instruction::type_function(
                    id,
                    lookup_function_type.return_type_id,
                    &lookup_function_type.parameter_type_ids,
                );
                instruction.to_words(&mut self.logical_layout.declarations);
                self.lookup_function_type.insert(lookup_function_type, id);
                id
            }
        }
    }

    fn write_composite_construct(
        &mut self,
        base_type_id: Word,
        constituent_ids: &[Word],
        block: &mut Block,
    ) -> Word {
        let id = self.generate_id();
        block.body.push(Instruction::composite_construct(
            base_type_id,
            id,
            constituent_ids,
        ));
        id
    }

    fn write_texture_coordinates(
        &mut self,
        ir_module: &crate::Module,
        coordinates: Handle<crate::Expression>,
        array_index: Option<Handle<crate::Expression>>,
        block: &mut Block,
    ) -> Result<Word, Error> {
        let coordinate_id = self.cached[coordinates];

        Ok(if let Some(array_index) = array_index {
            let coordinate_scalar_type_id = self.get_type_id(
                &ir_module.types,
                LookupType::Local(LocalType::Value {
                    vector_size: None,
                    kind: crate::ScalarKind::Float,
                    width: 4,
                    pointer_class: None,
                }),
            )?;

            let mut constituent_ids = [0u32; 4];
            let size = match *self.typifier.get(coordinates, &ir_module.types) {
                crate::TypeInner::Scalar { .. } => {
                    constituent_ids[0] = coordinate_id;
                    crate::VectorSize::Bi
                }
                crate::TypeInner::Vector { size, .. } => {
                    for i in 0..size as u32 {
                        let id = self.generate_id();
                        constituent_ids[i as usize] = id;
                        block.body.push(Instruction::composite_extract(
                            coordinate_scalar_type_id,
                            id,
                            coordinate_id,
                            &[i],
                        ));
                    }
                    match size {
                        crate::VectorSize::Bi => crate::VectorSize::Tri,
                        crate::VectorSize::Tri => crate::VectorSize::Quad,
                        crate::VectorSize::Quad => {
                            unimplemented!("Unable to extend the vec4 coordinate")
                        }
                    }
                }
                ref other => unimplemented!("wrong coordinate type {:?}", other),
            };

            let array_index_f32_id = self.generate_id();
            constituent_ids[size as usize - 1] = array_index_f32_id;

            let array_index_u32_id = self.cached[array_index];
            let cast_instruction = Instruction::unary(
                spirv::Op::ConvertUToF,
                coordinate_scalar_type_id,
                array_index_f32_id,
                array_index_u32_id,
            );
            block.body.push(cast_instruction);

            let extended_coordinate_type_id = self.get_type_id(
                &ir_module.types,
                LookupType::Local(LocalType::Value {
                    vector_size: Some(size),
                    kind: crate::ScalarKind::Float,
                    width: 4,
                    pointer_class: None,
                }),
            )?;

            self.write_composite_construct(
                extended_coordinate_type_id,
                &constituent_ids[..size as usize],
                block,
            )
        } else {
            coordinate_id
        })
    }

    /// Cache an expression for a value.
    fn cache_expression_value<'a>(
        &mut self,
        ir_module: &'a crate::Module,
        ir_function: &crate::Function,
        expr_handle: Handle<crate::Expression>,
        block: &mut Block,
        function: &mut Function,
    ) -> Result<(), Error> {
        let result_lookup_ty = match self.typifier.get_handle(expr_handle) {
            Ok(ty_handle) => LookupType::Handle(ty_handle),
            Err(inner) => LookupType::Local(LocalType::from_inner(inner).unwrap()),
        };
        let result_type_id = self.get_type_id(&ir_module.types, result_lookup_ty)?;

        let id = match ir_function.expressions[expr_handle] {
            crate::Expression::Access { base, index } => {
                let base_is_var = match ir_function.expressions[base] {
                    crate::Expression::GlobalVariable(_) | crate::Expression::LocalVariable(_) => {
                        true
                    }
                    _ => self.cached.ids[base.index()] == 0,
                };
                if base_is_var {
                    0
                } else {
                    let index_id = self.cached[index];
                    match *self.typifier.get(base, &ir_module.types) {
                        crate::TypeInner::Vector { .. } => {
                            let id = self.generate_id();
                            let base_id = self.cached[base];
                            block.body.push(Instruction::vector_extract_dynamic(
                                result_type_id,
                                id,
                                base_id,
                                index_id,
                            ));
                            id
                        }
                        //TODO: support `crate::TypeInner::Array { .. }` ?
                        ref other => {
                            log::error!("Unable to access {:?}", other);
                            return Err(Error::FeatureNotImplemented("access for type"));
                        }
                    }
                }
            }
            crate::Expression::AccessIndex { base, index } => {
                let base_is_var = match ir_function.expressions[base] {
                    crate::Expression::GlobalVariable(_) | crate::Expression::LocalVariable(_) => {
                        true
                    }
                    _ => self.cached.ids[base.index()] == 0,
                };
                if base_is_var {
                    0
                } else {
                    match *self.typifier.get(base, &ir_module.types) {
                        crate::TypeInner::Vector { .. }
                        | crate::TypeInner::Matrix { .. }
                        | crate::TypeInner::Array { .. }
                        | crate::TypeInner::Struct { .. } => {
                            let id = self.generate_id();
                            let base_id = self.cached[base];
                            block.body.push(Instruction::composite_extract(
                                result_type_id,
                                id,
                                base_id,
                                &[index],
                            ));
                            id
                        }
                        ref other => {
                            log::error!("Unable to access index of {:?}", other);
                            return Err(Error::FeatureNotImplemented("access index for type"));
                        }
                    }
                }
            }
            crate::Expression::GlobalVariable(handle) => self.global_variables[handle.index()].id,
            crate::Expression::Constant(handle) => self.lookup_constant[&handle],
            crate::Expression::Compose {
                ty: _,
                ref components,
            } => {
                //TODO: avoid allocation
                let mut constituent_ids = Vec::with_capacity(components.len());
                for &component in components {
                    let component_id = self.cached[component];
                    constituent_ids.push(component_id);
                }
                self.write_composite_construct(result_type_id, &constituent_ids, block)
            }
            crate::Expression::Unary { op, expr } => {
                let id = self.generate_id();
                let expr_id = self.cached[expr];
                let expr_ty_inner = self.typifier.get(expr, &ir_module.types);

                let spirv_op = match op {
                    crate::UnaryOperator::Negate => match expr_ty_inner.scalar_kind() {
                        Some(crate::ScalarKind::Float) => spirv::Op::FNegate,
                        Some(crate::ScalarKind::Sint) => spirv::Op::SNegate,
                        Some(crate::ScalarKind::Bool) => spirv::Op::LogicalNot,
                        Some(crate::ScalarKind::Uint) | None => {
                            log::error!("Unable to negate {:?}", expr_ty_inner);
                            return Err(Error::FeatureNotImplemented("negation"));
                        }
                    },
                    crate::UnaryOperator::Not => spirv::Op::Not,
                };

                block
                    .body
                    .push(Instruction::unary(spirv_op, result_type_id, id, expr_id));
                id
            }
            crate::Expression::Binary { op, left, right } => {
                let id = self.generate_id();
                let left_id = self.cached[left];
                let right_id = self.cached[right];

                let left_ty_inner = self.typifier.get(left, &ir_module.types);
                let right_ty_inner = self.typifier.get(right, &ir_module.types);

                let left_dimension = get_dimension(left_ty_inner);
                let right_dimension = get_dimension(right_ty_inner);

                let mut preserve_order = true;

                let spirv_op = match op {
                    crate::BinaryOperator::Add => match *left_ty_inner {
                        crate::TypeInner::Scalar { kind, .. }
                        | crate::TypeInner::Vector { kind, .. } => match kind {
                            crate::ScalarKind::Float => spirv::Op::FAdd,
                            _ => spirv::Op::IAdd,
                        },
                        _ => unimplemented!(),
                    },
                    crate::BinaryOperator::Subtract => match *left_ty_inner {
                        crate::TypeInner::Scalar { kind, .. }
                        | crate::TypeInner::Vector { kind, .. } => match kind {
                            crate::ScalarKind::Float => spirv::Op::FSub,
                            _ => spirv::Op::ISub,
                        },
                        _ => unimplemented!(),
                    },
                    crate::BinaryOperator::Multiply => match (left_dimension, right_dimension) {
                        (Dimension::Scalar, Dimension::Vector { .. }) => {
                            preserve_order = false;
                            spirv::Op::VectorTimesScalar
                        }
                        (Dimension::Vector, Dimension::Scalar { .. }) => {
                            spirv::Op::VectorTimesScalar
                        }
                        (Dimension::Vector, Dimension::Matrix) => spirv::Op::VectorTimesMatrix,
                        (Dimension::Matrix, Dimension::Scalar { .. }) => {
                            spirv::Op::MatrixTimesScalar
                        }
                        (Dimension::Matrix, Dimension::Vector) => spirv::Op::MatrixTimesVector,
                        (Dimension::Matrix, Dimension::Matrix) => spirv::Op::MatrixTimesMatrix,
                        (Dimension::Vector, Dimension::Vector)
                        | (Dimension::Scalar, Dimension::Scalar)
                            if left_ty_inner.scalar_kind() == Some(crate::ScalarKind::Float) =>
                        {
                            spirv::Op::FMul
                        }
                        (Dimension::Vector, Dimension::Vector)
                        | (Dimension::Scalar, Dimension::Scalar) => spirv::Op::IMul,
                        other => unimplemented!("Mul {:?}", other),
                    },
                    crate::BinaryOperator::Divide => match left_ty_inner.scalar_kind() {
                        Some(crate::ScalarKind::Sint) => spirv::Op::SDiv,
                        Some(crate::ScalarKind::Uint) => spirv::Op::UDiv,
                        Some(crate::ScalarKind::Float) => spirv::Op::FDiv,
                        _ => unimplemented!(),
                    },
                    crate::BinaryOperator::Modulo => match left_ty_inner.scalar_kind() {
                        Some(crate::ScalarKind::Sint) => spirv::Op::SMod,
                        Some(crate::ScalarKind::Uint) => spirv::Op::UMod,
                        Some(crate::ScalarKind::Float) => spirv::Op::FMod,
                        _ => unimplemented!(),
                    },
                    crate::BinaryOperator::Equal => match left_ty_inner.scalar_kind() {
                        Some(crate::ScalarKind::Sint) | Some(crate::ScalarKind::Uint) => {
                            spirv::Op::IEqual
                        }
                        Some(crate::ScalarKind::Float) => spirv::Op::FOrdEqual,
                        Some(crate::ScalarKind::Bool) => spirv::Op::LogicalEqual,
                        _ => unimplemented!(),
                    },
                    crate::BinaryOperator::NotEqual => match left_ty_inner.scalar_kind() {
                        Some(crate::ScalarKind::Sint) | Some(crate::ScalarKind::Uint) => {
                            spirv::Op::INotEqual
                        }
                        Some(crate::ScalarKind::Float) => spirv::Op::FOrdNotEqual,
                        Some(crate::ScalarKind::Bool) => spirv::Op::LogicalNotEqual,
                        _ => unimplemented!(),
                    },
                    crate::BinaryOperator::Less => match left_ty_inner.scalar_kind() {
                        Some(crate::ScalarKind::Sint) => spirv::Op::SLessThan,
                        Some(crate::ScalarKind::Uint) => spirv::Op::ULessThan,
                        Some(crate::ScalarKind::Float) => spirv::Op::FOrdLessThan,
                        _ => unimplemented!(),
                    },
                    crate::BinaryOperator::LessEqual => match left_ty_inner.scalar_kind() {
                        Some(crate::ScalarKind::Sint) => spirv::Op::SLessThanEqual,
                        Some(crate::ScalarKind::Uint) => spirv::Op::ULessThanEqual,
                        Some(crate::ScalarKind::Float) => spirv::Op::FOrdLessThanEqual,
                        _ => unimplemented!(),
                    },
                    crate::BinaryOperator::Greater => match left_ty_inner.scalar_kind() {
                        Some(crate::ScalarKind::Sint) => spirv::Op::SGreaterThan,
                        Some(crate::ScalarKind::Uint) => spirv::Op::UGreaterThan,
                        Some(crate::ScalarKind::Float) => spirv::Op::FOrdGreaterThan,
                        _ => unimplemented!(),
                    },
                    crate::BinaryOperator::GreaterEqual => match left_ty_inner.scalar_kind() {
                        Some(crate::ScalarKind::Sint) => spirv::Op::SGreaterThanEqual,
                        Some(crate::ScalarKind::Uint) => spirv::Op::UGreaterThanEqual,
                        Some(crate::ScalarKind::Float) => spirv::Op::FOrdGreaterThanEqual,
                        _ => unimplemented!(),
                    },
                    crate::BinaryOperator::And => spirv::Op::BitwiseAnd,
                    crate::BinaryOperator::ExclusiveOr => spirv::Op::BitwiseXor,
                    crate::BinaryOperator::InclusiveOr => spirv::Op::BitwiseOr,
                    crate::BinaryOperator::LogicalAnd => spirv::Op::LogicalAnd,
                    crate::BinaryOperator::LogicalOr => spirv::Op::LogicalOr,
                    crate::BinaryOperator::ShiftLeft => spirv::Op::ShiftLeftLogical,
                    crate::BinaryOperator::ShiftRight => match left_ty_inner.scalar_kind() {
                        Some(crate::ScalarKind::Sint) => spirv::Op::ShiftRightArithmetic,
                        Some(crate::ScalarKind::Uint) => spirv::Op::ShiftRightLogical,
                        _ => unimplemented!(),
                    },
                };

                block.body.push(Instruction::binary(
                    spirv_op,
                    result_type_id,
                    id,
                    if preserve_order { left_id } else { right_id },
                    if preserve_order { right_id } else { left_id },
                ));
                id
            }
            crate::Expression::Math {
                fun,
                arg,
                arg1,
                arg2,
            } => {
                use crate::MathFunction as Mf;
                enum MathOp {
                    Ext(spirv::GLOp),
                    Custom(Instruction),
                }

                let arg0_id = self.cached[arg];
                let arg_scalar_kind = self.typifier.get(arg, &ir_module.types).scalar_kind();
                let arg1_id = match arg1 {
                    Some(handle) => self.cached[handle],
                    None => 0,
                };
                let arg2_id = match arg2 {
                    Some(handle) => self.cached[handle],
                    None => 0,
                };

                let id = self.generate_id();
                let math_op = match fun {
                    // comparison
                    Mf::Abs => {
                        match arg_scalar_kind {
                            Some(crate::ScalarKind::Float) => MathOp::Ext(spirv::GLOp::FAbs),
                            Some(crate::ScalarKind::Sint) => MathOp::Ext(spirv::GLOp::SAbs),
                            Some(crate::ScalarKind::Uint) => {
                                MathOp::Custom(Instruction::unary(
                                    spirv::Op::CopyObject, // do nothing
                                    result_type_id,
                                    id,
                                    arg0_id,
                                ))
                            }
                            other => unimplemented!("Unexpected abs({:?})", other),
                        }
                    }
                    Mf::Min => MathOp::Ext(match arg_scalar_kind {
                        Some(crate::ScalarKind::Float) => spirv::GLOp::FMin,
                        Some(crate::ScalarKind::Sint) => spirv::GLOp::SMin,
                        Some(crate::ScalarKind::Uint) => spirv::GLOp::UMin,
                        other => unimplemented!("Unexpected min({:?})", other),
                    }),
                    Mf::Max => MathOp::Ext(match arg_scalar_kind {
                        Some(crate::ScalarKind::Float) => spirv::GLOp::FMax,
                        Some(crate::ScalarKind::Sint) => spirv::GLOp::SMax,
                        Some(crate::ScalarKind::Uint) => spirv::GLOp::UMax,
                        other => unimplemented!("Unexpected max({:?})", other),
                    }),
                    Mf::Clamp => MathOp::Ext(match arg_scalar_kind {
                        Some(crate::ScalarKind::Float) => spirv::GLOp::FClamp,
                        Some(crate::ScalarKind::Sint) => spirv::GLOp::SClamp,
                        Some(crate::ScalarKind::Uint) => spirv::GLOp::UClamp,
                        other => unimplemented!("Unexpected max({:?})", other),
                    }),
                    // trigonometry
                    Mf::Sin => MathOp::Ext(spirv::GLOp::Sin),
                    Mf::Sinh => MathOp::Ext(spirv::GLOp::Sinh),
                    Mf::Asin => MathOp::Ext(spirv::GLOp::Asin),
                    Mf::Cos => MathOp::Ext(spirv::GLOp::Cos),
                    Mf::Cosh => MathOp::Ext(spirv::GLOp::Cosh),
                    Mf::Acos => MathOp::Ext(spirv::GLOp::Acos),
                    Mf::Tan => MathOp::Ext(spirv::GLOp::Tan),
                    Mf::Tanh => MathOp::Ext(spirv::GLOp::Tanh),
                    Mf::Atan => MathOp::Ext(spirv::GLOp::Atan),
                    Mf::Atan2 => MathOp::Ext(spirv::GLOp::Atan2),
                    // decomposition
                    Mf::Ceil => MathOp::Ext(spirv::GLOp::Ceil),
                    Mf::Round => MathOp::Ext(spirv::GLOp::Round),
                    Mf::Floor => MathOp::Ext(spirv::GLOp::Floor),
                    Mf::Fract => MathOp::Ext(spirv::GLOp::Fract),
                    Mf::Trunc => MathOp::Ext(spirv::GLOp::Trunc),
                    Mf::Modf => MathOp::Ext(spirv::GLOp::Modf),
                    Mf::Frexp => MathOp::Ext(spirv::GLOp::Frexp),
                    Mf::Ldexp => MathOp::Ext(spirv::GLOp::Ldexp),
                    // geometry
                    Mf::Dot => MathOp::Custom(Instruction::binary(
                        spirv::Op::Dot,
                        result_type_id,
                        id,
                        arg0_id,
                        arg1_id,
                    )),
                    Mf::Cross => MathOp::Ext(spirv::GLOp::Cross),
                    Mf::Distance => MathOp::Ext(spirv::GLOp::Distance),
                    Mf::Length => MathOp::Ext(spirv::GLOp::Length),
                    Mf::Normalize => MathOp::Ext(spirv::GLOp::Normalize),
                    Mf::FaceForward => MathOp::Ext(spirv::GLOp::FaceForward),
                    Mf::Reflect => MathOp::Ext(spirv::GLOp::Reflect),
                    // exponent
                    Mf::Exp => MathOp::Ext(spirv::GLOp::Exp),
                    Mf::Exp2 => MathOp::Ext(spirv::GLOp::Exp2),
                    Mf::Log => MathOp::Ext(spirv::GLOp::Log),
                    Mf::Log2 => MathOp::Ext(spirv::GLOp::Log2),
                    Mf::Pow => MathOp::Ext(spirv::GLOp::Pow),
                    // computational
                    Mf::Sign => MathOp::Ext(match arg_scalar_kind {
                        Some(crate::ScalarKind::Float) => spirv::GLOp::FSign,
                        Some(crate::ScalarKind::Sint) => spirv::GLOp::SSign,
                        other => unimplemented!("Unexpected sign({:?})", other),
                    }),
                    Mf::Fma => MathOp::Ext(spirv::GLOp::Fma),
                    Mf::Mix => MathOp::Ext(spirv::GLOp::FMix),
                    Mf::Step => MathOp::Ext(spirv::GLOp::Step),
                    Mf::SmoothStep => MathOp::Ext(spirv::GLOp::SmoothStep),
                    Mf::Sqrt => MathOp::Ext(spirv::GLOp::Sqrt),
                    Mf::InverseSqrt => MathOp::Ext(spirv::GLOp::InverseSqrt),
                    Mf::Inverse => MathOp::Ext(spirv::GLOp::MatrixInverse),
                    Mf::Transpose => MathOp::Custom(Instruction::unary(
                        spirv::Op::Transpose,
                        result_type_id,
                        id,
                        arg0_id,
                    )),
                    Mf::Determinant => MathOp::Ext(spirv::GLOp::Determinant),
                    Mf::Outer | Mf::ReverseBits | Mf::CountOneBits => {
                        log::error!("unimplemented math function {:?}", fun);
                        return Err(Error::FeatureNotImplemented("math function"));
                    }
                };

                block.body.push(match math_op {
                    MathOp::Ext(op) => Instruction::ext_inst(
                        self.gl450_ext_inst_id,
                        op,
                        result_type_id,
                        id,
                        &[arg0_id, arg1_id, arg2_id][..fun.argument_count()],
                    ),
                    MathOp::Custom(inst) => inst,
                });
                id
            }
            crate::Expression::LocalVariable(variable) => function.variables[&variable].id,
            crate::Expression::Load { pointer } => {
                let (pointer_id, _) = self.write_expression_pointer(
                    ir_module,
                    ir_function,
                    pointer,
                    block,
                    function,
                )?;

                let id = self.generate_id();
                block
                    .body
                    .push(Instruction::load(result_type_id, id, pointer_id, None));
                id
            }
            crate::Expression::FunctionArgument(index) => {
                function.parameters[index as usize].result_id.unwrap()
            }
            crate::Expression::Call(_function) => self.lookup_function_call[&expr_handle],
            crate::Expression::As {
                expr,
                kind,
                convert,
            } => {
                let expr_id = self.cached[expr];
                let expr_kind = self
                    .typifier
                    .get(expr, &ir_module.types)
                    .scalar_kind()
                    .unwrap();

                let op = match (expr_kind, kind) {
                    _ if !convert => spirv::Op::Bitcast,
                    (crate::ScalarKind::Float, crate::ScalarKind::Uint) => spirv::Op::ConvertFToU,
                    (crate::ScalarKind::Float, crate::ScalarKind::Sint) => spirv::Op::ConvertFToS,
                    (crate::ScalarKind::Sint, crate::ScalarKind::Float) => spirv::Op::ConvertSToF,
                    (crate::ScalarKind::Uint, crate::ScalarKind::Float) => spirv::Op::ConvertUToF,
                    // We assume it's either an identity cast, or int-uint.
                    _ => spirv::Op::Bitcast,
                };

                let id = self.generate_id();
                let instruction = Instruction::unary(op, result_type_id, id, expr_id);
                block.body.push(instruction);
                id
            }
            crate::Expression::ImageLoad {
                image,
                coordinate,
                array_index,
                index,
            } => {
                let image_id = self.get_expression_global(ir_function, image);
                let coordinate_id =
                    self.write_texture_coordinates(ir_module, coordinate, array_index, block)?;

                let id = self.generate_id();

                let image_ty = self.typifier.get(image, &ir_module.types);
                let mut instruction = match *image_ty {
                    crate::TypeInner::Image {
                        class: crate::ImageClass::Storage { .. },
                        ..
                    } => Instruction::image_read(result_type_id, id, image_id, coordinate_id),
                    _ => Instruction::image_fetch(result_type_id, id, image_id, coordinate_id),
                };

                if let Some(index) = index {
                    let index_id = self.cached[index];
                    let image_ops = match *self.typifier.get(image, &ir_module.types) {
                        crate::TypeInner::Image {
                            class: crate::ImageClass::Sampled { multi: true, .. },
                            ..
                        } => spirv::ImageOperands::SAMPLE,
                        _ => spirv::ImageOperands::LOD,
                    };
                    instruction.add_operand(image_ops.bits());
                    instruction.add_operand(index_id);
                }

                block.body.push(instruction);
                id
            }
            crate::Expression::ImageSample {
                image,
                sampler,
                coordinate,
                array_index,
                offset,
                level,
                depth_ref,
            } => {
                use super::instructions::SampleLod;
                // image
                let image_id = self.get_expression_global(ir_function, image);
                let image_type = self.typifier.get_handle(image).unwrap();

                // OpTypeSampledImage
                let sampled_image_type_id = self.get_type_id(
                    &ir_module.types,
                    LookupType::Local(LocalType::SampledImage { image_type }),
                )?;

                let sampler_id = self.get_expression_global(ir_function, sampler);
                let coordinate_id =
                    self.write_texture_coordinates(ir_module, coordinate, array_index, block)?;

                let sampled_image_id = self.generate_id();
                block.body.push(Instruction::sampled_image(
                    sampled_image_type_id,
                    sampled_image_id,
                    image_id,
                    sampler_id,
                ));
                let id = self.generate_id();

                let depth_id = depth_ref.map(|handle| self.cached[handle]);

                let mut main_instruction = match level {
                    crate::SampleLevel::Zero => {
                        let mut inst = Instruction::image_sample(
                            result_type_id,
                            id,
                            SampleLod::Explicit,
                            sampled_image_id,
                            coordinate_id,
                            depth_id,
                        );

                        //TODO: cache this!
                        let zero_id = self.generate_id();
                        let zero_inner = crate::ConstantInner::Scalar {
                            width: 4,
                            value: crate::ScalarValue::Float(0.0),
                        };
                        self.write_constant_type(zero_id, &zero_inner, &ir_module.types)?;
                        inst.add_operand(spirv::ImageOperands::LOD.bits());
                        inst.add_operand(zero_id);

                        inst
                    }
                    crate::SampleLevel::Auto => Instruction::image_sample(
                        result_type_id,
                        id,
                        SampleLod::Implicit,
                        sampled_image_id,
                        coordinate_id,
                        depth_id,
                    ),
                    crate::SampleLevel::Exact(lod_handle) => {
                        let mut inst = Instruction::image_sample(
                            result_type_id,
                            id,
                            SampleLod::Explicit,
                            sampled_image_id,
                            coordinate_id,
                            depth_id,
                        );

                        let lod_id = self.cached[lod_handle];
                        inst.add_operand(spirv::ImageOperands::LOD.bits());
                        inst.add_operand(lod_id);

                        inst
                    }
                    crate::SampleLevel::Bias(bias_handle) => {
                        let mut inst = Instruction::image_sample(
                            result_type_id,
                            id,
                            SampleLod::Implicit,
                            sampled_image_id,
                            coordinate_id,
                            depth_id,
                        );

                        let bias_id = self.cached[bias_handle];
                        inst.add_operand(spirv::ImageOperands::BIAS.bits());
                        inst.add_operand(bias_id);

                        inst
                    }
                    crate::SampleLevel::Gradient { x, y } => {
                        let mut inst = Instruction::image_sample(
                            result_type_id,
                            id,
                            SampleLod::Explicit,
                            sampled_image_id,
                            coordinate_id,
                            depth_id,
                        );

                        let x_id = self.cached[x];
                        let y_id = self.cached[y];
                        inst.add_operand(spirv::ImageOperands::GRAD.bits());
                        inst.add_operand(x_id);
                        inst.add_operand(y_id);

                        inst
                    }
                };

                if let Some(offset_const) = offset {
                    let offset_id = self.lookup_constant[&offset_const];
                    main_instruction.add_operand(spirv::ImageOperands::CONST_OFFSET.bits());
                    main_instruction.add_operand(offset_id);
                }

                block.body.push(main_instruction);
                id
            }
            crate::Expression::Select {
                condition,
                accept,
                reject,
            } => {
                let id = self.generate_id();
                let condition_id = self.cached[condition];
                let accept_id = self.cached[accept];
                let reject_id = self.cached[reject];

                let instruction =
                    Instruction::select(result_type_id, id, condition_id, accept_id, reject_id);
                block.body.push(instruction);
                id
            }
            ref other => {
                log::error!("unimplemented {:?}", other);
                return Err(Error::FeatureNotImplemented("expression"));
            }
        };

        self.cached[expr_handle] = id;
        Ok(())
    }

    /// Write a left-hand-side expression, returning an `id` of the pointer.
    fn write_expression_pointer<'a>(
        &mut self,
        ir_module: &'a crate::Module,
        ir_function: &crate::Function,
        mut expr_handle: Handle<crate::Expression>,
        block: &mut Block,
        function: &mut Function,
    ) -> Result<(Word, spirv::StorageClass), Error> {
        let result_lookup_ty = match self.typifier.get_handle(expr_handle) {
            Ok(ty_handle) => LookupType::Handle(ty_handle),
            Err(inner) => LookupType::Local(LocalType::from_inner(inner).unwrap()),
        };
        let result_type_id = self.get_type_id(&ir_module.types, result_lookup_ty)?;

        self.temp_chain.clear();
        let (root_id, class) = loop {
            expr_handle = match ir_function.expressions[expr_handle] {
                crate::Expression::Access { base, index } => {
                    let index_id = self.cached[index];
                    self.temp_chain.push(index_id);
                    base
                }
                crate::Expression::AccessIndex { base, index } => {
                    let const_ty_id = self.get_type_id(
                        &ir_module.types,
                        LookupType::Local(LocalType::Value {
                            vector_size: None,
                            kind: crate::ScalarKind::Sint,
                            width: 4,
                            pointer_class: None,
                        }),
                    )?;
                    let const_id = self.create_constant(const_ty_id, &[index]);
                    self.temp_chain.push(const_id);
                    base
                }
                crate::Expression::GlobalVariable(handle) => {
                    let gv = &self.global_variables[handle.index()];
                    break (gv.id, gv.class);
                }
                crate::Expression::LocalVariable(variable) => {
                    let local_var = &function.variables[&variable];
                    break (local_var.id, spirv::StorageClass::Function);
                }
                ref other => unimplemented!("Unexpected pointer expression {:?}", other),
            }
        };

        let id = if self.temp_chain.is_empty() {
            root_id
        } else {
            self.temp_chain.reverse();
            let id = self.generate_id();
            block.body.push(Instruction::access_chain(
                result_type_id,
                id,
                root_id,
                &self.temp_chain,
            ));
            id
        };
        Ok((id, class))
    }

    fn get_expression_global(
        &self,
        ir_function: &crate::Function,
        expr_handle: Handle<crate::Expression>,
    ) -> Word {
        match ir_function.expressions[expr_handle] {
            crate::Expression::GlobalVariable(handle) => {
                let id = self.global_variables[handle.index()].handle_id;
                if id == 0 {
                    unreachable!("Global variable {:?} doesn't have a handle ID", handle);
                }
                id
            }
            ref other => unreachable!("Unexpected global expression {:?}", other),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write_block(
        &mut self,
        label_id: Word,
        statements: &[crate::Statement],
        ir_module: &crate::Module,
        ir_function: &crate::Function,
        function: &mut Function,
        exit_id: Option<Word>,
        loop_context: LoopContext,
    ) -> Result<(), Error> {
        let mut block = Block::new(label_id);

        for statement in statements {
            if block.termination.is_some() {
                unimplemented!("No statements are expected after block termination");
            }
            match *statement {
                crate::Statement::Emit(ref range) => {
                    for handle in range.clone() {
                        self.cache_expression_value(
                            ir_module,
                            ir_function,
                            handle,
                            &mut block,
                            function,
                        )?;
                    }
                }
                crate::Statement::Block(ref block_statements) => {
                    let scope_id = self.generate_id();
                    function.consume(block, Instruction::branch(scope_id));

                    let merge_id = self.generate_id();
                    self.write_block(
                        scope_id,
                        block_statements,
                        ir_module,
                        ir_function,
                        function,
                        Some(merge_id),
                        loop_context,
                    )?;

                    block = Block::new(merge_id);
                }
                crate::Statement::If {
                    condition,
                    ref accept,
                    ref reject,
                } => {
                    let condition_id = self.cached[condition];

                    let merge_id = self.generate_id();
                    block.body.push(Instruction::selection_merge(
                        merge_id,
                        spirv::SelectionControl::NONE,
                    ));

                    let accept_id = if accept.is_empty() {
                        None
                    } else {
                        Some(self.generate_id())
                    };
                    let reject_id = if reject.is_empty() {
                        None
                    } else {
                        Some(self.generate_id())
                    };

                    function.consume(
                        block,
                        Instruction::branch_conditional(
                            condition_id,
                            accept_id.unwrap_or(merge_id),
                            reject_id.unwrap_or(merge_id),
                        ),
                    );

                    if let Some(block_id) = accept_id {
                        self.write_block(
                            block_id,
                            accept,
                            ir_module,
                            ir_function,
                            function,
                            Some(merge_id),
                            loop_context,
                        )?;
                    }
                    if let Some(block_id) = reject_id {
                        self.write_block(
                            block_id,
                            reject,
                            ir_module,
                            ir_function,
                            function,
                            Some(merge_id),
                            loop_context,
                        )?;
                    }

                    block = Block::new(merge_id);
                }
                crate::Statement::Switch {
                    selector,
                    ref cases,
                    ref default,
                } => {
                    let selector_id = self.cached[selector];

                    let merge_id = self.generate_id();
                    block.body.push(Instruction::selection_merge(
                        merge_id,
                        spirv::SelectionControl::NONE,
                    ));

                    let default_id = self.generate_id();
                    let raw_cases = cases
                        .iter()
                        .map(|c| super::instructions::Case {
                            value: c.value as Word,
                            label_id: self.generate_id(),
                        })
                        .collect::<Vec<_>>();

                    function.consume(
                        block,
                        Instruction::switch(selector_id, default_id, &raw_cases),
                    );

                    for (i, (case, raw_case)) in cases.iter().zip(raw_cases.iter()).enumerate() {
                        let case_finish_id = if case.fall_through {
                            match raw_cases.get(i + 1) {
                                Some(rc) => rc.label_id,
                                None => default_id,
                            }
                        } else {
                            merge_id
                        };
                        self.write_block(
                            raw_case.label_id,
                            &case.body,
                            ir_module,
                            ir_function,
                            function,
                            Some(case_finish_id),
                            LoopContext::default(),
                        )?;
                    }

                    self.write_block(
                        default_id,
                        default,
                        ir_module,
                        ir_function,
                        function,
                        Some(merge_id),
                        LoopContext::default(),
                    )?;

                    block = Block::new(merge_id);
                }
                crate::Statement::Loop {
                    ref body,
                    ref continuing,
                } => {
                    let preamble_id = self.generate_id();
                    function.consume(block, Instruction::branch(preamble_id));

                    let merge_id = self.generate_id();
                    let body_id = self.generate_id();
                    let continuing_id = self.generate_id();

                    // SPIR-V requires the continuing to the `OpLoopMerge`,
                    // so we have to start a new block with it.
                    block = Block::new(preamble_id);
                    block.body.push(Instruction::loop_merge(
                        merge_id,
                        continuing_id,
                        spirv::SelectionControl::NONE,
                    ));
                    function.consume(block, Instruction::branch(body_id));

                    self.write_block(
                        body_id,
                        body,
                        ir_module,
                        ir_function,
                        function,
                        Some(continuing_id),
                        LoopContext {
                            continuing_id: Some(continuing_id),
                            break_id: Some(merge_id),
                        },
                    )?;

                    self.write_block(
                        continuing_id,
                        continuing,
                        ir_module,
                        ir_function,
                        function,
                        Some(preamble_id),
                        LoopContext {
                            continuing_id: None,
                            break_id: Some(merge_id),
                        },
                    )?;

                    block = Block::new(merge_id);
                }
                crate::Statement::Break => {
                    block.termination = Some(Instruction::branch(loop_context.break_id.unwrap()));
                }
                crate::Statement::Continue => {
                    block.termination =
                        Some(Instruction::branch(loop_context.continuing_id.unwrap()));
                }
                crate::Statement::Return { value: Some(value) } => {
                    let id = self.cached[value];
                    block.termination = Some(Instruction::return_value(id));
                }
                crate::Statement::Return { value: None } => {
                    block.termination = Some(Instruction::return_void());
                }
                crate::Statement::Kill => {
                    block.termination = Some(Instruction::kill());
                }
                crate::Statement::Store { pointer, value } => {
                    let (pointer_id, _) = self.write_expression_pointer(
                        ir_module,
                        ir_function,
                        pointer,
                        &mut block,
                        function,
                    )?;
                    let value_id = self.cached[value];

                    block
                        .body
                        .push(Instruction::store(pointer_id, value_id, None));
                }
                crate::Statement::ImageStore {
                    image,
                    coordinate,
                    array_index,
                    value,
                } => {
                    let image_id = self.get_expression_global(ir_function, image);
                    let coordinate_id = self.write_texture_coordinates(
                        ir_module,
                        coordinate,
                        array_index,
                        &mut block,
                    )?;
                    let value_id = self.cached[value];

                    block
                        .body
                        .push(Instruction::image_write(image_id, coordinate_id, value_id));
                }
                crate::Statement::Call {
                    function: local_function,
                    ref arguments,
                    result,
                } => {
                    let id = self.generate_id();
                    //TODO: avoid heap allocation
                    let mut argument_ids = vec![];

                    for &argument in arguments {
                        let arg_id = self.cached[argument];
                        argument_ids.push(arg_id);
                    }

                    let type_id = match result {
                        Some(expr) => {
                            self.cached[expr] = id;
                            self.lookup_function_call.insert(expr, id);
                            let ty_handle =
                                ir_module.functions[local_function].return_type.unwrap();
                            self.get_type_id(&ir_module.types, LookupType::Handle(ty_handle))?
                        }
                        None => self.void_type,
                    };

                    block.body.push(Instruction::function_call(
                        type_id,
                        id,
                        self.lookup_function[&local_function],
                        argument_ids.as_slice(),
                    ));
                }
            }
        }

        if block.termination.is_none() {
            block.termination = Some(match exit_id {
                Some(id) => Instruction::branch(id),
                None => Instruction::return_void(),
            });
        }

        function.blocks.push(block);
        Ok(())
    }

    fn write_physical_layout(&mut self) {
        self.physical_layout.bound = self.id_count + 1;
    }

    fn write_logical_layout(
        &mut self,
        ir_module: &crate::Module,
        analysis: &Analysis,
    ) -> Result<(), Error> {
        Instruction::type_void(self.void_type).to_words(&mut self.logical_layout.declarations);
        Instruction::ext_inst_import(self.gl450_ext_inst_id, "GLSL.std.450")
            .to_words(&mut self.logical_layout.ext_inst_imports);

        if self.flags.contains(WriterFlags::DEBUG) {
            self.debugs
                .push(Instruction::source(spirv::SourceLanguage::GLSL, 450));
        }

        for (handle, constant) in ir_module.constants.iter() {
            let id = self.generate_id();
            self.lookup_constant.insert(handle, id);
            if self.flags.contains(WriterFlags::DEBUG) {
                if let Some(ref name) = constant.name {
                    self.debugs.push(Instruction::name(id, name));
                }
            }
            self.write_constant_type(id, &constant.inner, &ir_module.types)?;
        }

        self.global_variables.clear();
        for (handle, var) in ir_module.global_variables.iter() {
            if let crate::TypeInner::Struct { .. } = ir_module.types[var.ty].inner {
                self.struct_type_handles.insert(var.ty, var.storage_access);
            }
            let (instruction, id, class) = self.write_global_variable(ir_module, handle)?;
            instruction.to_words(&mut self.logical_layout.declarations);
            self.global_variables.push(GlobalVariable {
                id,
                handle_id: 0,
                class,
            });
        }

        for (handle, ir_function) in ir_module.functions.iter() {
            let info = &analysis[handle];
            let id = self.write_function(ir_function, info, ir_module)?;
            self.lookup_function.insert(handle, id);
        }

        for (ep_index, ir_ep) in ir_module.entry_points.iter().enumerate() {
            let info = analysis.get_entry_point(ep_index);
            let ep_instruction = self.write_entry_point(ir_ep, info, ir_module)?;
            ep_instruction.to_words(&mut self.logical_layout.entry_points);
        }

        for capability in self.capabilities.iter() {
            Instruction::capability(*capability).to_words(&mut self.logical_layout.capabilities);
        }

        let addressing_model = spirv::AddressingModel::Logical;
        let memory_model = spirv::MemoryModel::GLSL450;
        self.check(addressing_model.required_capabilities())?;
        self.check(memory_model.required_capabilities())?;

        Instruction::memory_model(addressing_model, memory_model)
            .to_words(&mut self.logical_layout.memory_model);

        if self.flags.contains(WriterFlags::DEBUG) {
            for debug in self.debugs.iter() {
                debug.to_words(&mut self.logical_layout.debugs);
            }
        }

        for annotation in self.annotations.iter() {
            annotation.to_words(&mut self.logical_layout.annotations);
        }

        Ok(())
    }

    pub fn write(
        &mut self,
        ir_module: &crate::Module,
        analysis: &Analysis,
        words: &mut Vec<Word>,
    ) -> Result<(), Error> {
        self.lookup_function.clear();
        self.lookup_function_type.clear();
        self.lookup_function_call.clear();

        self.layouter
            .initialize(&ir_module.types, &ir_module.constants);

        self.write_logical_layout(ir_module, analysis)?;
        self.write_physical_layout();

        self.physical_layout.in_words(words);
        self.logical_layout.in_words(words);
        Ok(())
    }
}

#[test]
fn test_writer_generate_id() {
    let mut writer = Writer::new(&Options::default()).unwrap();
    assert_eq!(writer.id_count, 2);
    writer.generate_id();
    assert_eq!(writer.id_count, 3);
}

#[test]
fn test_write_physical_layout() {
    let mut writer = Writer::new(&Options::default()).unwrap();
    assert_eq!(writer.physical_layout.bound, 0);
    writer.write_physical_layout();
    assert_eq!(writer.physical_layout.bound, 3);
}
