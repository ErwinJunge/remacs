//! Functions operating on buffers.

use libc::{self, c_char, c_int, c_uchar, c_void, ptrdiff_t};
use std::{self, mem, ptr};

use remacs_macros::lisp_fn;

use crate::{
    character::char_head_p,
    chartable::LispCharTableRef,
    data::Lisp_Fwd,
    editfns::point,
    frames::LispFrameRef,
    lisp::defsubr,
    lisp::{ExternalPtr, LispObject, LiveBufferIter},
    lists::{car, cdr, list, member},
    marker::{marker_buffer, marker_position_lisp, set_marker_both, LispMarkerRef},
    multibyte::{multibyte_length_by_head, string_char},
    numbers::MOST_POSITIVE_FIXNUM,
    remacs_sys::{
        allocate_misc, bset_update_mode_line, buffer_local_flags, buffer_local_value,
        buffer_window_count, del_range, delete_all_overlays, drop_overlay, globals,
        last_per_buffer_idx, set_buffer_internal_1, specbind, unbind_to, unchain_both,
        update_mode_lines,
    },
    remacs_sys::{
        pvec_type, EmacsInt, Lisp_Buffer, Lisp_Buffer_Local_Value, Lisp_Misc_Type, Lisp_Overlay,
        Lisp_Type, Vbuffer_alist,
    },
    remacs_sys::{
        windows_or_buffers_changed, Fcopy_sequence, Fexpand_file_name, Ffind_file_name_handler,
        Fget_text_property, Fnconc, Fnreverse, Foverlay_get, Fwiden,
    },
    remacs_sys::{
        Qafter_string, Qbefore_string, Qbuffer_read_only, Qbufferp, Qget_file_buffer,
        Qinhibit_quit, Qinhibit_read_only, Qnil, Qoverlayp, Qt, Qunbound, Qvoid_variable,
    },
    strings::string_equal,
    threads::{c_specpdl_index, ThreadState},
};

pub const BEG: ptrdiff_t = 1;
pub const BEG_BYTE: ptrdiff_t = 1;

/// Return value of point, in bytes, as an integer.
/// Beginning of buffer is position (point-min).
pub fn point_byte() -> EmacsInt {
    let buffer_ref = ThreadState::current_buffer();
    buffer_ref.pt_byte as EmacsInt
}

/// Return the minimum permissible byte_position in the current
/// buffer.  This is 1, unless narrowing (a buffer restriction) is in
/// effect.
pub fn point_min_byte() -> EmacsInt {
    ThreadState::current_buffer().begv_byte as EmacsInt
}

/// Maximum number of bytes in a buffer.
/// A buffer cannot contain more bytes than a 1-origin fixnum can
/// represent, nor can it be so large that C pointer arithmetic stops
/// working. The `ptrdiff_t` cast ensures that this is signed, not unsigned.
//const fn buf_bytes_max() -> ptrdiff_t {
//    const mpf: ptrdiff_t = (MOST_POSITIVE_FIXNUM - 1) as ptrdiff_t;
//    const eimv: ptrdiff_t = EmacsInt::max_value() as ptrdiff_t;
//    const pdmv: ptrdiff_t = libc::ptrdiff_t::max_value();
//    const arith_max: ptrdiff_t = if eimv <= pdmv {
//        eimv
//    } else {
//        pdmv
//    };
//    if mpf as ptrdiff_t <= arith_max {
//        mpf
//    } else {
//        arith_max
//    }
//}
// TODO(db48x): use the nicer implementation above once const functions can have conditionals in them
// https://github.com/rust-lang/rust/issues/24111
const fn buf_bytes_max() -> ptrdiff_t {
    const p: [ptrdiff_t; 2] = [
        EmacsInt::max_value() as ptrdiff_t,
        libc::ptrdiff_t::max_value(),
    ];
    const arith_max: ptrdiff_t =
        p[((p[1] - p[0]) >> ((8 * std::mem::size_of::<ptrdiff_t>()) - 1)) as usize];
    const q: [ptrdiff_t; 2] = [(MOST_POSITIVE_FIXNUM - 1) as ptrdiff_t, arith_max];
    q[((q[1] - q[0]) >> ((8 * std::mem::size_of::<ptrdiff_t>()) - 1)) as usize]
}
pub const BUF_BYTES_MAX: ptrdiff_t = buf_bytes_max();

pub type LispBufferRef = ExternalPtr<Lisp_Buffer>;
pub type LispOverlayRef = ExternalPtr<Lisp_Overlay>;

impl LispBufferRef {
    pub fn as_lisp_obj(self) -> LispObject {
        LispObject::tag_ptr(self, Lisp_Type::Lisp_Vectorlike)
    }

    pub fn is_read_only(self) -> bool {
        self.read_only_.into()
    }

    pub fn beg(self) -> ptrdiff_t {
        BEG
    }

    pub fn beg_byte(self) -> ptrdiff_t {
        BEG_BYTE
    }

    pub fn gap_start_addr(self) -> *mut c_uchar {
        unsafe { (*self.text).beg.offset((*self.text).gpt_byte - BEG_BYTE) }
    }

    pub fn gap_end_addr(self) -> *mut c_uchar {
        unsafe {
            (*self.text)
                .beg
                .offset((*self.text).gpt_byte + (*self.text).gap_size - BEG_BYTE)
        }
    }

    pub fn z_addr(self) -> *mut c_uchar {
        unsafe {
            (*self.text)
                .beg
                .offset((*self.text).gap_size + (*self.text).z_byte - BEG_BYTE)
        }
    }

    pub fn markers(self) -> Option<LispMarkerRef> {
        unsafe { (*self.text).markers.as_ref().map(|m| mem::transmute(m)) }
    }

    pub fn mark_active(self) -> LispObject {
        self.mark_active_
    }

    pub fn pt_marker(self) -> LispObject {
        self.pt_marker_
    }

    pub fn begv_marker(self) -> LispObject {
        self.begv_marker_
    }

    pub fn zv_marker(self) -> LispObject {
        self.zv_marker_
    }

    pub fn mark(self) -> LispObject {
        self.mark_
    }

    #[allow(dead_code)]
    pub fn name(self) -> LispObject {
        self.name_
    }

    pub fn filename(self) -> LispObject {
        self.filename_
    }

    pub fn base_buffer(self) -> Option<LispBufferRef> {
        Self::from_ptr(self.base_buffer as *mut c_void)
    }

    pub fn truename(self) -> LispObject {
        self.file_truename_
    }

    pub fn case_fold_search(self) -> LispObject {
        self.case_fold_search_
    }

    // Check if buffer is live
    pub fn is_live(self) -> bool {
        self.name_.is_not_nil()
    }

    pub fn set_pt_both(&mut self, charpos: ptrdiff_t, byte: ptrdiff_t) {
        self.pt = charpos;
        self.pt_byte = byte;
    }

    pub fn set_begv_both(&mut self, charpos: ptrdiff_t, byte: ptrdiff_t) {
        self.begv = charpos;
        self.begv_byte = byte;
    }

    pub fn set_zv_both(&mut self, charpos: ptrdiff_t, byte: ptrdiff_t) {
        self.zv = charpos;
        self.zv_byte = byte;
    }

    pub fn set_syntax_table(&mut self, table: LispCharTableRef) {
        self.syntax_table_ = LispObject::from(table);
    }

    pub fn value_p(self, idx: isize) -> bool {
        if idx < 0 || idx >= (unsafe { last_per_buffer_idx } as isize) {
            panic!("buffer value_p called with an invalid index!");
        }
        self.local_flags[idx as usize] != 0
    }

    // Similar to SET_PER_BUFFER_VALUE_P macro in C
    /// Set whether per-buffer variable with index IDX has a buffer-local
    /// value in buffer.  VAL zero means it does't.
    pub fn set_per_buffer_value_p(&mut self, idx: usize, val: libc::c_char) {
        unsafe {
            if idx >= last_per_buffer_idx as usize {
                panic!(
                    "set_per_buffer_value_p called with index greater than {}",
                    last_per_buffer_idx
                );
            }
        }
        self.local_flags[idx] = val;
    }

    // Characters, positions and byte positions.

    /// Return the address of byte position N in current buffer.
    pub fn byte_pos_addr(self, n: ptrdiff_t) -> *mut c_uchar {
        unsafe { (*self.text).beg.offset(n - BEG_BYTE) }
    }

    /// Return the address of character at byte position BYTE_POS.
    pub fn buf_byte_address(self, byte_pos: isize) -> c_uchar {
        let gap = self.pos_within_range(byte_pos);
        unsafe { *(self.beg_addr().offset(byte_pos - BEG_BYTE + gap)) as c_uchar }
    }

    /// Return the byte at byte position N.
    pub fn fetch_byte(self, n: ptrdiff_t) -> u8 {
        let offset = if n >= self.gpt_byte() {
            self.gap_size()
        } else {
            0
        };

        unsafe { *(self.beg_addr().offset(offset + n - self.beg_byte())) as u8 }
    }

    /// Return character at byte position POS.  See the caveat WARNING for
    /// FETCH_MULTIBYTE_CHAR below.
    pub fn fetch_char(self, n: ptrdiff_t) -> c_int {
        if self.multibyte_characters_enabled() {
            self.fetch_multibyte_char(n)
        } else {
            c_int::from(self.fetch_byte(n))
        }
    }

    /// Return character code of multi-byte form at byte position POS.  If POS
    /// doesn't point the head of valid multi-byte form, only the byte at
    /// POS is returned.  No range checking.
    pub fn fetch_multibyte_char(self, n: ptrdiff_t) -> c_int {
        let offset = if n >= self.gpt_byte() && n >= 0 {
            self.gap_size()
        } else {
            0
        };

        unsafe {
            string_char(
                self.beg_addr().offset(offset + n - self.beg_byte()),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        }
    }

    pub fn multibyte_characters_enabled(self) -> bool {
        self.enable_multibyte_characters_.is_not_nil()
    }

    pub fn pos_within_range(self, pos: isize) -> isize {
        if pos >= self.gpt_byte() {
            self.gap_size()
        } else {
            0
        }
    }

    // Same as the BUF_INC_POS c macro
    /// Increment the buffer byte position POS_BYTE of the the buffer to
    /// the next character boundary.  This macro relies on the fact that
    /// *GPT_ADDR and *Z_ADDR are always accessible and the values are
    /// '\0'.  No range checking of POS_BYTE.
    pub fn inc_pos(self, pos_byte: isize) -> isize {
        let chp = self.buf_byte_address(pos_byte);
        pos_byte + multibyte_length_by_head(chp) as isize
    }

    // Same as the BUF_DEC_POS c macro
    /// Decrement the buffer byte position POS_BYTE of the buffer to
    /// the previous character boundary.  No range checking of POS_BYTE.
    pub fn dec_pos(self, pos_byte: isize) -> isize {
        let mut new_pos = pos_byte - 1;
        let mut offset = new_pos - self.beg_byte();
        offset += self.pos_within_range(new_pos);
        unsafe {
            let mut chp = self.beg_addr().offset(offset);

            while !char_head_p(*chp) {
                chp = chp.offset(-1);
                new_pos -= 1;
            }
        }
        new_pos
    }

    // Methods for accessing struct buffer_text fields

    pub fn beg_addr(self) -> *mut c_uchar {
        unsafe { (*self.text).beg }
    }

    pub fn gpt(self) -> ptrdiff_t {
        unsafe { (*self.text).gpt }
    }

    pub fn gpt_byte(self) -> ptrdiff_t {
        unsafe { (*self.text).gpt_byte }
    }

    pub fn gap_size(self) -> ptrdiff_t {
        unsafe { (*self.text).gap_size }
    }

    pub fn gap_position(self) -> ptrdiff_t {
        unsafe { (*self.text).gpt }
    }

    /// Number of modifications made to the buffer.
    pub fn modifications(self) -> EmacsInt {
        unsafe { (*self.text).modiff }
    }

    /// Value of `modiff` last time the buffer was saved.
    pub fn modifications_since_save(self) -> EmacsInt {
        unsafe { (*self.text).save_modiff }
    }

    /// Number of modifications to the buffer's characters.
    pub fn char_modifications(self) -> EmacsInt {
        unsafe { (*self.text).chars_modiff }
    }

    pub fn z_byte(self) -> ptrdiff_t {
        unsafe { (*self.text).z_byte }
    }

    pub fn z(self) -> ptrdiff_t {
        unsafe { (*self.text).z }
    }

    pub fn overlays_before(self) -> Option<LispOverlayRef> {
        unsafe { self.overlays_before.as_ref().map(|m| mem::transmute(m)) }
    }

    pub fn overlays_after(self) -> Option<LispOverlayRef> {
        unsafe { self.overlays_after.as_ref().map(|m| mem::transmute(m)) }
    }

    pub fn as_live(self) -> Option<LispBufferRef> {
        if self.is_live() {
            Some(self)
        } else {
            None
        }
    }

    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn set_value(&mut self, offset: usize, value: LispObject) {
        let buffer_bytes = self.as_mut() as *mut c_char;
        let pos = buffer_bytes.add(offset) as *mut LispObject;
        *pos = value;
    }
}

impl LispObject {
    pub fn is_buffer(self) -> bool {
        self.as_vectorlike()
            .map_or(false, |v| v.is_pseudovector(pvec_type::PVEC_BUFFER))
    }

    pub fn as_buffer(self) -> Option<LispBufferRef> {
        self.as_vectorlike().and_then(|v| v.as_buffer())
    }

    pub fn as_live_buffer(self) -> Option<LispBufferRef> {
        self.as_buffer().and_then(|b| b.as_live())
    }

    pub fn as_buffer_or_error(self) -> LispBufferRef {
        self.as_buffer()
            .unwrap_or_else(|| wrong_type!(Qbufferp, self))
    }
}

impl From<LispObject> for LispBufferRef {
    fn from(o: LispObject) -> Self {
        o.as_buffer_or_error()
    }
}

impl From<LispBufferRef> for LispObject {
    fn from(b: LispBufferRef) -> Self {
        b.as_lisp_obj()
    }
}

impl From<LispObject> for Option<LispBufferRef> {
    fn from(o: LispObject) -> Self {
        o.as_buffer()
    }
}

impl LispObject {
    pub fn is_overlay(self) -> bool {
        self.as_misc()
            .map_or(false, |m| m.get_type() == Lisp_Misc_Type::Lisp_Misc_Overlay)
    }

    pub fn as_overlay(self) -> Option<LispOverlayRef> {
        self.as_misc().and_then(|m| {
            if m.get_type() == Lisp_Misc_Type::Lisp_Misc_Overlay {
                unsafe { Some(mem::transmute(m)) }
            } else {
                None
            }
        })
    }

    pub fn as_overlay_or_error(self) -> LispOverlayRef {
        self.as_overlay()
            .unwrap_or_else(|| wrong_type!(Qoverlayp, self))
    }
}

impl From<LispObject> for LispOverlayRef {
    fn from(o: LispObject) -> Self {
        o.as_overlay_or_error()
    }
}

impl From<LispOverlayRef> for LispObject {
    fn from(o: LispOverlayRef) -> Self {
        o.as_lisp_obj()
    }
}

impl From<LispObject> for Option<LispOverlayRef> {
    fn from(o: LispObject) -> Self {
        o.as_overlay()
    }
}

impl LispOverlayRef {
    pub fn as_lisp_obj(self) -> LispObject {
        LispObject::tag_ptr(self, Lisp_Type::Lisp_Misc)
    }

    pub fn iter(self) -> LispOverlayIter {
        LispOverlayIter {
            current: Some(self),
        }
    }
}

pub struct LispOverlayIter {
    current: Option<LispOverlayRef>,
}

impl Iterator for LispOverlayIter {
    type Item = LispOverlayRef;

    fn next(&mut self) -> Option<Self::Item> {
        let c = self.current;
        match c {
            None => None,
            Some(o) => {
                self.current = LispOverlayRef::from_ptr(o.next as *mut c_void);
                c
            }
        }
    }
}

impl LispObject {
    /// Return SELF as a struct buffer pointer, defaulting to the current buffer.
    /// Same as the decode_buffer function in buffer.h
    pub fn as_buffer_or_current_buffer(self) -> LispBufferRef {
        if self.is_nil() {
            ThreadState::current_buffer()
        } else {
            self.as_buffer_or_error()
        }
    }
}

pub type LispBufferLocalValueRef = ExternalPtr<Lisp_Buffer_Local_Value>;

impl LispBufferLocalValueRef {
    pub fn get_fwd(self) -> *const Lisp_Fwd {
        self.fwd
    }

    pub fn get_value(self) -> LispObject {
        self.valcell.as_cons_or_error().cdr()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LispBufferOrName {
    Buffer(LispObject),
    Name(LispObject),
}

impl LispBufferOrName {
    pub fn as_buffer(self) -> Option<LispBufferRef> {
        self.into()
    }

    pub fn as_buffer_or_current_buffer(self) -> Option<LispBufferRef> {
        let obj = LispObject::from(self);
        obj.map_or_else(|| Some(ThreadState::current_buffer()), |o| o.as_buffer())
    }
}

impl From<LispBufferOrName> for LispObject {
    fn from(buffer_or_name: LispBufferOrName) -> LispObject {
        match buffer_or_name {
            LispBufferOrName::Buffer(b) => b,
            LispBufferOrName::Name(n) => n,
        }
    }
}

impl From<LispObject> for LispBufferOrName {
    fn from(v: LispObject) -> LispBufferOrName {
        if v.is_string() {
            LispBufferOrName::Name(v)
        } else {
            v.as_buffer_or_error();
            LispBufferOrName::Buffer(v)
        }
    }
}

impl From<LispObject> for Option<LispBufferOrName> {
    fn from(v: LispObject) -> Option<LispBufferOrName> {
        if v.is_nil() {
            None
        } else if v.is_string() {
            Some(LispBufferOrName::Name(v))
        } else if v.is_buffer() {
            Some(LispBufferOrName::Buffer(v))
        } else {
            None
        }
    }
}

impl From<LispBufferOrName> for Option<LispBufferRef> {
    fn from(v: LispBufferOrName) -> Option<LispBufferRef> {
        let buffer = match v {
            LispBufferOrName::Buffer(b) => b,
            LispBufferOrName::Name(n) => {
                cdr(assoc_ignore_text_properties(n, unsafe { Vbuffer_alist }))
            }
        };
        buffer.as_buffer()
    }
}

impl From<LispBufferOrName> for LispBufferRef {
    fn from(v: LispBufferOrName) -> LispBufferRef {
        v.as_buffer().unwrap_or_else(|| nsberror(v.into()))
    }
}

pub struct LispBufferOrCurrent(LispBufferRef);

impl From<LispObject> for LispBufferOrCurrent {
    fn from(obj: LispObject) -> LispBufferOrCurrent {
        LispBufferOrCurrent(obj.as_buffer_or_current_buffer())
    }
}

impl From<LispBufferOrCurrent> for LispObject {
    fn from(buffer: LispBufferOrCurrent) -> LispObject {
        buffer.unwrap().into()
    }
}

impl LispBufferOrCurrent {
    pub fn unwrap(self) -> LispBufferRef {
        self.0
    }
}

/// Return a list of all existing live buffers.
/// If the optional arg FRAME is a frame, we return the buffer list in the
/// proper order for that frame: the buffers show in FRAME come first,
/// followed by the rest of the buffers.
#[lisp_fn(min = "0")]
pub fn buffer_list(frame: Option<LispFrameRef>) -> LispObject {
    let mut buffers: Vec<LispObject> = unsafe { Vbuffer_alist }.iter_cars_safe().map(cdr).collect();

    match frame {
        None => list(&buffers),

        Some(frame) => {
            let framelist = unsafe { Fcopy_sequence(frame.buffer_list) };
            let prevlist = unsafe { Fnreverse(Fcopy_sequence(frame.buried_buffer_list)) };

            // Remove any buffer that duplicates one in FRAMELIST or PREVLIST.
            buffers.retain(|e| member(*e, framelist) == Qnil && member(*e, prevlist) == Qnil);

            callN_raw!(Fnconc, framelist, list(&buffers), prevlist)
        }
    }
}

/// Return t if OBJECT is an overlay.
#[lisp_fn]
pub fn overlayp(object: LispObject) -> bool {
    object.is_overlay()
}

/// Return non-nil if OBJECT is a buffer which has not been killed.
/// Value is nil if OBJECT is not a buffer or if it has been killed.
#[lisp_fn]
pub fn buffer_live_p(object: Option<LispBufferRef>) -> bool {
    object.map_or(false, |m| m.is_live())
}

/// Like Fassoc, but use `Fstring_equal` to compare
/// (which ignores text properties), and don't ever quit.
fn assoc_ignore_text_properties(key: LispObject, list: LispObject) -> LispObject {
    let result = list
        .iter_tails_safe()
        .find(|&item| string_equal(car(item.car()), key));
    match result {
        Some(elt) => elt.car(),
        None => Qnil,
    }
}

/// Return the buffer named BUFFER-OR-NAME.
/// BUFFER-OR-NAME must be either a string or a buffer.  If BUFFER-OR-NAME
/// is a string and there is no buffer with that name, return nil.  If
/// BUFFER-OR-NAME is a buffer, return it as given.
#[lisp_fn]
pub fn get_buffer(buffer_or_name: LispBufferOrName) -> Option<LispBufferRef> {
    buffer_or_name.into()
}

/// Return the current buffer as a Lisp object.
#[lisp_fn]
pub fn current_buffer() -> LispObject {
    ThreadState::current_buffer().as_lisp_obj()
}

/// Return name of file BUFFER is visiting, or nil if none.
/// No argument or nil as argument means use the current buffer.
#[lisp_fn(min = "0")]
pub fn buffer_file_name(buffer: LispBufferOrCurrent) -> LispObject {
    let buf = buffer.unwrap();

    buf.filename_
}

/// Return t if BUFFER was modified since its file was last read or saved.
/// No argument or nil as argument means use current buffer as BUFFER.
#[lisp_fn(min = "0")]
pub fn buffer_modified_p(buffer: LispBufferOrCurrent) -> bool {
    let buf = buffer.unwrap();
    buf.modifications_since_save() < buf.modifications()
}

/// Return the name of BUFFER, as a string.
/// BUFFER defaults to the current buffer.
/// Return nil if BUFFER has been killed.
#[lisp_fn(min = "0")]
pub fn buffer_name(buffer: LispBufferOrCurrent) -> LispObject {
    let buf = buffer.unwrap();
    buf.name_
}

/// Return BUFFER's tick counter, incremented for each change in text.
/// Each buffer has a tick counter which is incremented each time the
/// text in that buffer is changed.  It wraps around occasionally.
/// No argument or nil as argument means use current buffer as BUFFER.
#[lisp_fn(min = "0")]
pub fn buffer_modified_tick(buffer: LispBufferOrCurrent) -> EmacsInt {
    let buf = buffer.unwrap();
    buf.modifications()
}

/// Return BUFFER's character-change tick counter.
/// Each buffer has a character-change tick counter, which is set to the
/// value of the buffer's tick counter (see `buffer-modified-tick'), each
/// time text in that buffer is inserted or deleted.  By comparing the
/// values returned by two individual calls of `buffer-chars-modified-tick',
/// you can tell whether a character change occurred in that buffer in
/// between these calls.  No argument or nil as argument means use current
/// buffer as BUFFER.
#[lisp_fn(min = "0")]
pub fn buffer_chars_modified_tick(buffer: LispBufferOrCurrent) -> EmacsInt {
    let buf = buffer.unwrap();
    buf.char_modifications()
}

/// Return the position at which OVERLAY starts.
#[lisp_fn]
pub fn overlay_start(overlay: LispOverlayRef) -> Option<EmacsInt> {
    marker_position_lisp(overlay.start.into())
}

/// Return the position at which OVERLAY ends.
#[lisp_fn]
pub fn overlay_end(overlay: LispOverlayRef) -> Option<EmacsInt> {
    marker_position_lisp(overlay.end.into())
}

/// Return the buffer OVERLAY belongs to.
/// Return nil if OVERLAY has been deleted.
#[lisp_fn]
pub fn overlay_buffer(overlay: LispOverlayRef) -> Option<LispBufferRef> {
    marker_buffer(overlay.start.into())
}

/// Return a list of the properties on OVERLAY.
/// This is a copy of OVERLAY's plist; modifying its conses has no
/// effect on OVERLAY.
#[lisp_fn]
pub fn overlay_properties(overlay: LispOverlayRef) -> LispObject {
    unsafe { Fcopy_sequence(overlay.plist) }
}

#[no_mangle]
pub unsafe extern "C" fn validate_region(b: *mut LispObject, e: *mut LispObject) {
    let start = *b;
    let stop = *e;

    let mut beg = start.as_fixnum_coerce_marker_or_error();
    let mut end = stop.as_fixnum_coerce_marker_or_error();

    if beg > end {
        mem::swap(&mut beg, &mut end);
    }

    *b = LispObject::from(beg);
    *e = LispObject::from(end);

    let buf = ThreadState::current_buffer();
    let begv = buf.begv as EmacsInt;
    let zv = buf.zv as EmacsInt;

    if !(begv <= beg && end <= zv) {
        args_out_of_range!(current_buffer(), start, stop);
    }
}

/// Make buffer BUFFER-OR-NAME current for editing operations.
/// BUFFER-OR-NAME may be a buffer or the name of an existing buffer.
/// See also `with-current-buffer' when you want to make a buffer current
/// temporarily.  This function does not display the buffer, so its effect
/// ends when the current command terminates.  Use `switch-to-buffer' or
/// `pop-to-buffer' to switch buffers permanently.
/// The return value is the buffer made current.
#[lisp_fn]
pub fn set_buffer(buffer_or_name: LispBufferOrName) -> LispBufferRef {
    let mut buffer: LispBufferRef = buffer_or_name.into();
    if !buffer.is_live() {
        error!("Selecting deleted buffer");
    };
    unsafe { set_buffer_internal_1(buffer.as_mut()) };
    buffer
}

/// Signal a `buffer-read-only' error if the current buffer is read-only.
/// If the text under POSITION (which defaults to point) has the
/// `inhibit-read-only' text property set, the error will not be raised.
#[lisp_fn(min = "0")]
pub fn barf_if_buffer_read_only(position: Option<EmacsInt>) {
    let pos = position.unwrap_or_else(point);

    let inhibit_read_only: bool = unsafe { globals.Vinhibit_read_only.into() };
    let prop = unsafe { Fget_text_property(LispObject::from(pos), Qinhibit_read_only, Qnil) };

    if ThreadState::current_buffer().is_read_only() && !inhibit_read_only && prop.is_nil() {
        xsignal!(Qbuffer_read_only, current_buffer())
    }
}

/// No such buffer error.
#[no_mangle]
pub extern "C" fn nsberror(spec: LispObject) -> ! {
    match spec.as_string() {
        Some(s) => error!("No buffer named {}", s),
        None => error!("Invalid buffer argument"),
    }
}

/// These functions are for debugging overlays.

/// Return a pair of lists giving all the overlays of the current buffer.
/// The car has all the overlays before the overlay center;
/// the cdr has all the overlays after the overlay center.
/// Recentering overlays moves overlays between these lists.
/// The lists you get are copies, so that changing them has no effect.
/// However, the overlays you get are the real objects that the buffer uses.
#[lisp_fn]
pub fn overlay_lists() -> LispObject {
    let list_overlays = |ol: LispOverlayRef| -> LispObject {
        ol.iter()
            .fold(Qnil, |accum, n| LispObject::cons(n.as_lisp_obj(), accum))
    };

    let cur_buf = ThreadState::current_buffer();
    let before = cur_buf.overlays_before().map_or(Qnil, &list_overlays);
    let after = cur_buf.overlays_after().map_or(Qnil, &list_overlays);
    unsafe { LispObject::cons(Fnreverse(before), Fnreverse(after)) }
}

fn get_truename_buffer_1(filename: LispObject) -> LispObject {
    LiveBufferIter::new()
        .find(|buf| {
            let buf_truename = buf.truename();
            buf_truename.is_string() && string_equal(buf_truename, filename)
        })
        .into()
}

#[no_mangle]
pub extern "C" fn get_truename_buffer(filename: LispObject) -> LispObject {
    get_truename_buffer_1(filename)
}

/// If buffer B has markers to record PT, BEGV and ZV when it is not
/// current, update these markers.
#[no_mangle]
pub extern "C" fn record_buffer_markers(buffer: *mut Lisp_Buffer) {
    let buffer_ref = LispBufferRef::from_ptr(buffer as *mut c_void)
        .unwrap_or_else(|| panic!("Invalid buffer reference."));
    let pt_marker = buffer_ref.pt_marker();

    if pt_marker.is_not_nil() {
        let begv_marker = buffer_ref.begv_marker();
        let zv_marker = buffer_ref.zv_marker();

        assert!(begv_marker.is_not_nil());
        assert!(zv_marker.is_not_nil());

        let buffer = buffer_ref.as_lisp_obj();
        set_marker_both(pt_marker, buffer, buffer_ref.pt, buffer_ref.pt_byte);
        set_marker_both(begv_marker, buffer, buffer_ref.begv, buffer_ref.begv_byte);
        set_marker_both(zv_marker, buffer, buffer_ref.zv, buffer_ref.zv_byte);
    }
}

/// If buffer B has markers to record PT, BEGV and ZV when it is not
/// current, fetch these values into B->begv etc.
#[no_mangle]
pub extern "C" fn fetch_buffer_markers(buffer: *mut Lisp_Buffer) {
    let mut buffer_ref = LispBufferRef::from_ptr(buffer as *mut c_void)
        .unwrap_or_else(|| panic!("Invalid buffer reference."));

    if buffer_ref.pt_marker().is_not_nil() {
        assert!(buffer_ref.begv_marker().is_not_nil());
        assert!(buffer_ref.zv_marker().is_not_nil());

        let pt_marker = buffer_ref.pt_marker().as_marker_or_error();
        let begv_marker = buffer_ref.begv_marker().as_marker_or_error();
        let zv_marker = buffer_ref.zv_marker().as_marker_or_error();

        buffer_ref.set_pt_both(pt_marker.charpos_or_error(), pt_marker.bytepos_or_error());
        buffer_ref.set_begv_both(
            begv_marker.charpos_or_error(),
            begv_marker.bytepos_or_error(),
        );
        buffer_ref.set_zv_both(zv_marker.charpos_or_error(), zv_marker.bytepos_or_error());
    }
}

/// Return the buffer visiting file FILENAME (a string).
/// The buffer's `buffer-file-name' must match exactly the expansion of FILENAME.
/// If there is no such live buffer, return nil.
/// See also `find-buffer-visiting'.
#[lisp_fn]
pub fn get_file_buffer(filename: LispObject) -> Option<LispBufferRef> {
    verify_lisp_type!(filename, Qstringp);
    let filename = unsafe { Fexpand_file_name(filename, Qnil) };

    // If the file name has special constructs in it,
    // call the corresponding file handler.
    let handler = unsafe { Ffind_file_name_handler(filename, Qget_file_buffer) };

    if handler.is_not_nil() {
        let handled_buf = call!(handler, Qget_file_buffer, filename);
        handled_buf.as_buffer()
    } else {
        LiveBufferIter::new().find(|buf| {
            let buf_filename = buf.filename();
            buf_filename.is_string() && string_equal(buf_filename, filename)
        })
    }
}

/// Return the value of VARIABLE in BUFFER.
/// If VARIABLE does not have a buffer-local binding in BUFFER, the value
/// is the default binding of the variable.
#[lisp_fn(name = "buffer-local-value", c_name = "buffer_local_value")]
pub fn buffer_local_value_lisp(variable: LispObject, buffer: LispObject) -> LispObject {
    let result = unsafe { buffer_local_value(variable, buffer) };

    if result.eq(Qunbound) {
        xsignal!(Qvoid_variable, variable);
    }

    result
}

/// Return the base buffer of indirect buffer BUFFER.
/// If BUFFER is not indirect, return nil.
/// BUFFER defaults to the current buffer.
#[lisp_fn(min = "0")]
pub fn buffer_base_buffer(buffer: LispBufferOrCurrent) -> Option<LispBufferRef> {
    let buf = buffer.unwrap();
    buf.base_buffer()
}

/// Force redisplay of the current buffer's mode line and header line.
/// With optional non-nil ALL, force redisplay of all mode lines and
/// header lines.  This function also forces recomputation of the
/// menu bar menus and the frame title.
#[lisp_fn(min = "0")]
pub fn force_mode_line_update(all: bool) -> bool {
    let mut current_buffer = ThreadState::current_buffer();
    if all {
        unsafe {
            update_mode_lines = 10;
        }
        // FIXME: This can't be right.
        current_buffer.set_prevent_redisplay_optimizations_p(true);
    } else if 0 < unsafe { buffer_window_count(current_buffer.as_mut()) } {
        unsafe {
            bset_update_mode_line(current_buffer.as_mut());
        }
        current_buffer.set_prevent_redisplay_optimizations_p(true);
    }
    all
}

/// Return a Lisp_Misc_Overlay object with specified START, END and PLIST.
#[no_mangle]
pub extern "C" fn build_overlay(
    start: LispObject,
    end: LispObject,
    plist: LispObject,
) -> LispObject {
    unsafe {
        let obj = allocate_misc(Lisp_Misc_Type::Lisp_Misc_Overlay);
        let mut overlay = obj.as_overlay_or_error();
        overlay.start = start;
        overlay.end = end;
        overlay.plist = plist;
        overlay.next = ptr::null_mut();

        overlay.as_lisp_obj()
    }
}

/// Delete the overlay OVERLAY from its buffer.
#[lisp_fn]
pub fn delete_overlay(overlay: LispObject) {
    let mut ov_ref = overlay.as_overlay_or_error();
    let mut buf_ref = match marker_buffer(ov_ref.start.as_marker_or_error()) {
        Some(b) => b,
        None => return,
    };
    let count = c_specpdl_index();

    unsafe {
        specbind(Qinhibit_quit, Qt);
        unchain_both(buf_ref.as_mut(), overlay);
        drop_overlay(buf_ref.as_mut(), ov_ref.as_mut());

        // When deleting an overlay with before or after strings, turn off
        // display optimizations for the affected buffer, on the basis that
        // these strings may contain newlines.  This is easier to do than to
        // check for that situation during redisplay.
        if windows_or_buffers_changed != 0 && Foverlay_get(overlay, Qbefore_string).is_not_nil()
            || Foverlay_get(overlay, Qafter_string).is_not_nil()
        {
            buf_ref.set_prevent_redisplay_optimizations_p(true);
        }
    }

    unsafe { unbind_to(count, Qnil) };
}

/// Delete all overlays of BUFFER.
/// BUFFER omitted or nil means delete all overlays of the current buffer.
#[lisp_fn(min = "0", name = "delete-all-overlays")]
pub fn delete_all_overlays_lisp(buffer: LispBufferOrCurrent) {
    unsafe { delete_all_overlays(buffer.unwrap().as_mut()) };
}

/// Delete the entire contents of the current buffer.
/// Any narrowing restriction in effect (see `narrow-to-region') is removed,
/// so the buffer is truly empty after this.
#[lisp_fn(intspec = "*")]
pub fn erase_buffer() {
    unsafe {
        Fwiden();

        let mut cur_buf = ThreadState::current_buffer();
        del_range(cur_buf.beg(), cur_buf.z());

        cur_buf.last_window_start = 1;

        // Prevent warnings, or suspension of auto saving, that would happen
        // if future size is less than past size.  Use of erase-buffer
        // implies that the future text is not really related to the past text.
        cur_buf.save_length_ = LispObject::from(0);
    }
}

pub unsafe fn per_buffer_idx(offset: isize) -> isize {
    let flags = &mut buffer_local_flags as *mut Lisp_Buffer as *mut LispObject;
    let obj = flags.offset(offset);
    (*obj).as_fixnum_or_error() as isize
}

#[no_mangle]
pub extern "C" fn rust_syms_of_buffer() {
    def_lisp_sym!(Qget_file_buffer, "get-file-buffer");

    /// Analogous to `mode-line-format', but controls the header line.
    /// The header line appears, optionally, at the top of a window;
    /// the mode line appears at the bottom.
    defvar_per_buffer!(header_line_format_, "header-line-format", Qnil);
}

include!(concat!(env!("OUT_DIR"), "/buffers_exports.rs"));
