use std::collections::HashMap;
use std::collections::hash_map::Entry::*;
use std::iter::Extend;
use std::mem;
use std::io;

use byteorder::{ByteOrder, LittleEndian};
use take_mut;

use ::{DynasmApi, DynasmLabelApi};
use ::common::{BaseAssembler, UncommittedModifier};
use ::{ExecutableBuffer, MutableBuffer, Executor, DynamicLabel, AssemblyOffset};

// the argument to each relocation is the amount of bytes between the end
// of the actual relocation and the moment the relocation got emitted
// This has to be this way due to the insanity that is x64 encoding
#[derive(Debug, Clone, Copy)]
pub enum Relocation {
    Byte(u8),
    Word(u8),
    DWord(u8),
    QWord(u8)
}

impl Relocation {
    fn size(&self) -> usize {
        match *self {
            Relocation::Byte(_) => 1,
            Relocation::Word(_)  => 2,
            Relocation::DWord(_) => 4,
            Relocation::QWord(_) => 8,
        }
    }

    fn offset(&self) -> usize {
        (match *self {
            Relocation::Byte(o)  => o,
            Relocation::Word(o)  => o,
            Relocation::DWord(o) => o,
            Relocation::QWord(o) => o
        }) as usize
    }
}

#[derive(Debug)]
struct PatchLoc(usize, Relocation);

/// This struct is an implementation of a dynasm runtime. It supports incremental
/// compilation as well as multithreaded execution with simultaneous compilation.
/// Its implementation ensures that no memory is writeable and executable at the
/// same time.
#[derive(Debug)]
pub struct Assembler {
    // protection swapping executable buffer
    base: BaseAssembler,

    // label name -> target loc
    global_labels: HashMap<&'static str, usize>,
    // end of patch location -> name
    global_relocs: Vec<(PatchLoc, &'static str)>,

    // label id -> target loc
    dynamic_labels: Vec<Option<usize>>,
    // location to be resolved, loc, label id
    dynamic_relocs: Vec<(PatchLoc, DynamicLabel)>,

    // labelname -> most recent patch location
    local_labels: HashMap<&'static str, usize>,
    // locations to be patched once this label gets seen. name -> Vec<locs>
    local_relocs: HashMap<&'static str, Vec<PatchLoc>>
}

/// the default starting size for an allocation by this assembler.
/// This is the page size on x64 platforms.
const MMAP_INIT_SIZE: usize = 4096;

impl Assembler {
    /// Create a new `Assembler` instance
    /// This function will return an error if it was not
    /// able to map the required executable memory. However, further methods
    /// on the `Assembler` will simply panic if an error occurs during memory
    /// remapping as otherwise it would violate the invariants of the assembler.
    /// This behaviour could be improved but currently the underlying memmap crate
    /// does not return the original mappings if a call to mprotect/VirtualProtect
    /// fails so there is no reliable way to error out if a call fails while leaving
    /// the logic of the `Assembler` intact.
    pub fn new() -> io::Result<Assembler> {
        Ok(Assembler {
            base: BaseAssembler::new(MMAP_INIT_SIZE)?,
            global_labels: HashMap::new(),
            dynamic_labels: Vec::new(),
            local_labels: HashMap::new(),
            global_relocs: Vec::new(),
            dynamic_relocs: Vec::new(),
            local_relocs: HashMap::new()
        })
    }

    /// Create a new dynamic label that can be referenced and defined.
    pub fn new_dynamic_label(&mut self) -> DynamicLabel {
        let id = self.dynamic_labels.len();
        self.dynamic_labels.push(None);
        DynamicLabel(id)
    }

    /// To allow already committed code to be altered, this method allows modification
    /// of the internal ExecutableBuffer directly. When this method is called, all
    /// data will be committed and access to the internal `ExecutableBuffer` will be locked.
    /// The passed function will then be called with an `AssemblyModifier` as argument.
    /// Using this `AssemblyModifier` changes can be made to the committed code.
    /// After this function returns, any labels in these changes will be resolved
    /// and the `ExecutableBuffer` will be unlocked again.
    pub fn alter<F>(&mut self, f: F) where F: FnOnce(&mut AssemblyModifier) -> () {
        self.commit();

        let cloned = self.base.reader();
        let mut lock = cloned.write().unwrap();

        // move the buffer out of the assembler for a bit
        take_mut::take_or_recover(&mut *lock, || ExecutableBuffer::new(0, MMAP_INIT_SIZE).unwrap(), |buf| {
            let mut buf = buf.make_mut().unwrap();

            {
                let mut m = AssemblyModifier {
                    asmoffset: 0,
                    assembler: self,
                    buffer: &mut buf
                };
                f(&mut m);
                m.encode_relocs();
            }

            // and stuff it back in
            buf.make_exec().unwrap()
        });

        // no commit is required as we directly modified the buffer.
    }

    /// Similar to `Assembler::alter`, this method allows modification of the yet to be
    /// committed assembing buffer. Note that it is not possible to use labels in this
    /// context, and overriding labels will cause corruption when the assembler tries to
    /// resolve the labels at commit time.
    pub fn alter_uncommitted(&mut self) -> UncommittedModifier {
        self.base.alter_uncommitted()
    }

    #[inline]
    fn patch_loc(&mut self, loc: PatchLoc, target: usize) {
        // the value that the relocation will have
        let target = target as isize - loc.0 as isize;

        // slice out the part of the buffer to be overwritten with said value
        let offset = loc.0 - self.base.asmoffset() - loc.1.offset();
        let buf = &mut self.base.ops[offset - loc.1.size() .. offset];

        match loc.1 {
            Relocation::Byte(_)  => buf[0] = target as i8 as u8,
            Relocation::Word(_)  => LittleEndian::write_i16(buf, target as i16),
            Relocation::DWord(_) => LittleEndian::write_i32(buf, target as i32),
            Relocation::QWord(_) => LittleEndian::write_i64(buf, target as i64)
        }
    }

    fn encode_relocs(&mut self) {
        let mut relocs = Vec::new();
        mem::swap(&mut relocs, &mut self.global_relocs);
        for (loc, name) in relocs {
            if let Some(&target) = self.global_labels.get(&name) {
                self.patch_loc(loc, target)
            } else {
                panic!("Unknown global label '{}'", name);
            }
        }

        let mut relocs = Vec::new();
        mem::swap(&mut relocs, &mut self.dynamic_relocs);
        for (loc, id) in relocs {
            if let Some(&Some(target)) = self.dynamic_labels.get(id.0) {
                self.patch_loc(loc, target)
            } else {
                panic!("Unknown dynamic label '{}'", id.0);
            }
        }

        if let Some(name) = self.local_relocs.keys().next() {
            panic!("Unknown local label '{}'", name);
        }
    }

    /// Commit the assembled code from a temporary buffer to the executable buffer.
    /// This method requires write access to the execution buffer and therefore
    /// has to obtain a lock on the datastructure. When this method is called, all
    /// labels will be resolved, and the result can no longer be changed.
    pub fn commit(&mut self) {
        // finalize all relocs in the newest part.
        self.encode_relocs();

        // update the executable buffer
        self.base.commit();
    }

    /// Consumes the assembler to return the internal ExecutableBuffer. This
    /// method will only fail if an `Executor` currently holds a lock on the datastructure,
    /// in which case it will return itself.
    pub fn finalize(mut self) -> Result<ExecutableBuffer, Assembler> {
        self.commit();
        match self.base.finalize() {
            Ok(execbuffer) => Ok(execbuffer),
            Err(base) => Err(Assembler {
                base: base,
                ..self
            })
        }
    }

    /// Creates a read-only reference to the internal `ExecutableBuffer` that must
    /// be locked to access it. Multiple of such read-only locks can be obtained
    /// at the same time, but as long as they are alive they will block any `self.commit()`
    /// calls.
    pub fn reader(&self) -> Executor {
        Executor {
            execbuffer: self.base.reader()
        }
    }
}

impl DynasmApi for Assembler {
    #[inline]
    fn offset(&self) -> AssemblyOffset {
        AssemblyOffset(self.base.offset())
    }

    #[inline]
    fn push(&mut self, value: u8) {
        self.base.push(value);
    }
}

impl DynasmLabelApi for Assembler {
    type Relocation = Relocation;

    #[inline]
    fn align(&mut self, alignment: usize) {
        self.base.align(alignment, 0x90);
    }

    #[inline]
    fn global_label(&mut self, name: &'static str) {
        let offset = self.offset().0;
        if let Some(name) = self.global_labels.insert(name, offset) {
            panic!("Duplicate global label '{}'", name);
        }
    }

    #[inline]
    fn global_reloc(&mut self, name: &'static str, kind: Relocation) {
        let offset = self.offset().0;
        self.global_relocs.push((PatchLoc(offset, kind), name));
    }

    #[inline]
    fn dynamic_label(&mut self, id: DynamicLabel) {
        let offset = self.offset().0;
        let entry = &mut self.dynamic_labels[id.0];
        if entry.is_some() {
            panic!("Duplicate label '{}'", id.0);
        }
        *entry = Some(offset);
    }

    #[inline]
    fn dynamic_reloc(&mut self, id: DynamicLabel, kind: Relocation) {
        let offset = self.offset().0;
        self.dynamic_relocs.push((PatchLoc(offset, kind), id));
    }

    #[inline]
    fn local_label(&mut self, name: &'static str) {
        let offset = self.offset().0;
        if let Some(relocs) = self.local_relocs.remove(&name) {
            for loc in relocs {
                self.patch_loc(loc, offset);
            }
        }
        self.local_labels.insert(name, offset);
    }

    #[inline]
    fn forward_reloc(&mut self, name: &'static str, kind: Relocation) {
        let offset = self.offset().0;
        match self.local_relocs.entry(name) {
            Occupied(mut o) => {
                o.get_mut().push(PatchLoc(offset, kind));
            },
            Vacant(v) => {
                v.insert(vec![PatchLoc(offset, kind)]);
            }
        }
    }

    #[inline]
    fn backward_reloc(&mut self, name: &'static str, kind: Relocation) {
        if let Some(&target) = self.local_labels.get(&name) {
            let len = self.offset().0;
            self.patch_loc(PatchLoc(len, kind), target)
        } else {
            panic!("Unknown local label '{}'", name);
        }
    }
}

impl Extend<u8> for Assembler {
    #[inline]
    fn extend<T>(&mut self, iter: T) where T: IntoIterator<Item=u8> {
        self.base.extend(iter)
    }
}

impl<'a> Extend<&'a u8> for Assembler {
    #[inline]
    fn extend<T>(&mut self, iter: T) where T: IntoIterator<Item=&'a u8> {
        self.base.extend(iter)
    }
}


/// This struct is a wrapper around an `Assembler` normally created using the
/// `Assembler.alter` method. Instead of writing to a temporary assembling buffer,
/// this struct assembles directly into an executable buffer. The `goto` method can
/// be used to set the assembling offset in the `ExecutableBuffer` of the assembler
/// (this offset is initialized to 0) after which the data at this location can be
/// overwritten by assembling into this struct.
pub struct AssemblyModifier<'a: 'b, 'b> {
    assembler: &'a mut Assembler,
    buffer: &'b mut MutableBuffer,
    asmoffset: usize
}

impl<'a, 'b> AssemblyModifier<'a, 'b> {
    /// Sets the current modification offset to the given value
    #[inline]
    pub fn goto(&mut self, offset: AssemblyOffset) {
        self.asmoffset = offset.0;
    }

    /// Checks that the current modification offset is not larger than the specified offset.
    /// If this is violated, it panics.
    #[inline]
    pub fn check(&mut self, offset: AssemblyOffset) {
        if self.asmoffset > offset.0 {
            panic!("specified offset to check is smaller than the actual offset");
        }
    }

    /// Checks that the current modification offset is exactly the specified offset.
    /// If this is violated, it panics.
    #[inline]
    pub fn check_exact(&mut self, offset: AssemblyOffset) {
        if self.asmoffset != offset.0 {
            panic!("specified offset to check is not the actual offset");
        }
    }

    #[inline]
    fn patch_loc(&mut self, loc: PatchLoc, target: usize) {
        // the value that the relocation will have
        let target = target as isize - loc.0 as isize;

        // slice out the part of the buffer to be overwritten with said value
        let offset = loc.0 - loc.1.offset();
        let buf = &mut self.buffer[offset - loc.1.size() .. offset];

        match loc.1 {
            Relocation::Byte(_)  => buf[0] = target as i8 as u8,
            Relocation::Word(_)  => LittleEndian::write_i16(buf, target as i16),
            Relocation::DWord(_) => LittleEndian::write_i32(buf, target as i32),
            Relocation::QWord(_) => LittleEndian::write_i64(buf, target as i64)
        }
    }

    fn encode_relocs(&mut self) {
        let mut relocs = Vec::new();
        mem::swap(&mut relocs, &mut self.assembler.global_relocs);
        for (loc, name) in relocs {
            if let Some(&target) = self.assembler.global_labels.get(&name) {
                self.patch_loc(loc, target)
            } else {
                panic!("Unkonwn global label '{}'", name);
            }
        }

        let mut relocs = Vec::new();
        mem::swap(&mut relocs, &mut self.assembler.dynamic_relocs);
        for (loc, id) in relocs {
            if let Some(&Some(target)) = self.assembler.dynamic_labels.get(id.0) {
                self.patch_loc(loc, target)
            } else {
                panic!("Unkonwn dynamic label '{}'", id.0);
            }
        }

        if let Some(name) = self.assembler.local_relocs.keys().next() {
            panic!("Unknown local label '{}'", name);
        }
    }
}

impl<'a, 'b> DynasmApi for AssemblyModifier<'a, 'b> {
    #[inline]
    fn offset(&self) -> AssemblyOffset {
        AssemblyOffset(self.asmoffset)
    }

    #[inline]
    fn push(&mut self, value: u8) {
        self.buffer[self.asmoffset] = value;
        self.asmoffset += 1;
    }
}

impl<'a, 'b> DynasmLabelApi for AssemblyModifier<'a, 'b> {
    type Relocation = Relocation;

    #[inline]
    fn align(&mut self, alignment: usize) {
        self.assembler.align(alignment);
    }

    #[inline]
    fn global_label(&mut self, name: &'static str) {
        self.assembler.global_label(name);
    }

    #[inline]
    fn global_reloc(&mut self, name: &'static str, kind: Relocation) {
        self.assembler.global_reloc(name, kind);
    }

    #[inline]
    fn dynamic_label(&mut self, id: DynamicLabel) {
        self.assembler.dynamic_label(id);
    }

    #[inline]
    fn dynamic_reloc(&mut self, id: DynamicLabel, kind: Relocation) {
        self.assembler.dynamic_reloc(id, kind);
    }

    #[inline]
    fn local_label(&mut self, name: &'static str) {
        let offset = self.offset().0;
        if let Some(relocs) = self.assembler.local_relocs.remove(&name) {
            for loc in relocs {
                self.patch_loc(loc, offset);
            }
        }
        self.assembler.local_labels.insert(name, offset);
    }

    #[inline]
    fn forward_reloc(&mut self, name: &'static str, kind: Relocation) {
        self.assembler.forward_reloc(name, kind);
    }

    #[inline]
    fn backward_reloc(&mut self, name: &'static str, kind: Relocation) {
        if let Some(&target) = self.assembler.local_labels.get(&name) {
            let len = self.offset().0;
            self.patch_loc(PatchLoc(len, kind), target)
        } else {
            panic!("Unknown local label '{}'", name);
        }
    }
}

impl<'a, 'b> Extend<u8> for AssemblyModifier<'a, 'b> {
    #[inline]
    fn extend<T>(&mut self, iter: T) where T: IntoIterator<Item=u8> {
        for i in iter {
            self.push(i)
        }
    }
}

impl<'a, 'b, 'c> Extend<&'c u8> for AssemblyModifier<'a, 'b> {
    #[inline]
    fn extend<T>(&mut self, iter: T) where T: IntoIterator<Item=&'c u8> {
        self.extend(iter.into_iter().cloned())
    }
}
