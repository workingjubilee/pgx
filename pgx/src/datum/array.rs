/*
Portions Copyright 2019-2021 ZomboDB, LLC.
Portions Copyright 2021-2022 Technology Concepts & Design, Inc. <support@tcdi.com>

All rights reserved.

Use of this source code is governed by the MIT license that can be found in the LICENSE file.
*/

use crate::array::RawArray;
use crate::layout::*;
use crate::{pg_sys, FromDatum, IntoDatum, PgMemoryContexts};
use bitvec::slice::BitSlice;
use core::ptr::NonNull;
use pgx_utils::sql_entity_graph::metadata::{
    ArgumentError, Returns, ReturnsError, SqlMapping, SqlTranslatable,
};
use serde::Serializer;
use std::marker::PhantomData;
use std::{mem, ptr, slice};

pub struct Array<'a, T: FromDatum> {
    ptr: NonNull<pg_sys::varlena>,
    raw: Option<RawArray>,
    nelems: usize,
    // Remove this field if/when we figure out how to stop using pg_sys::deconstruct_array
    datum_palloc: Option<NonNull<pg_sys::Datum>>,
    datum_slice: Option<&'a [pg_sys::Datum]>,
    elems_ptr: Option<NonNull<T>>,
    null_slice: NullKind<'a>,
    elem_layout: Layout,
    _marker: PhantomData<T>,
}

// FIXME: When Array::over gets removed, this enum can probably be dropped
// since we won't be entertaining ArrayTypes which don't use bitslices anymore.
// However, we could also use a static resolution? Hard to say what's best.
enum NullKind<'a> {
    Bits(&'a BitSlice<u8>),
    Strict(usize),
}

impl NullKind<'_> {
    fn get(&self, index: usize) -> Option<bool> {
        match self {
            Self::Bits(b1) => b1.get(index).map(|b| !b),
            Self::Strict(len) => index.le(len).then(|| false),
        }
    }

    fn any(&self) -> bool {
        match self {
            Self::Bits(b1) => !b1.all(),
            Self::Strict(_) => false,
        }
    }
}

impl<'a, T: FromDatum + serde::Serialize> serde::Serialize for Array<'a, T> {
    fn serialize<S>(&self, serializer: S) -> Result<<S as Serializer>::Ok, <S as Serializer>::Error>
    where
        S: Serializer,
    {
        serializer.collect_seq(self.iter())
    }
}

impl<'a, T: FromDatum> Drop for Array<'a, T> {
    fn drop(&mut self) {
        if let Array { raw, datum_palloc: Some(data), datum_slice, .. } = self {
            // If Drop has arrived here, it means that this Array is backed by an allocation or two
            // If so, the first one is guaranteed, and was created by calling pg_sys::deconstruct_array
            // This is just a slice, dropping it doesn't "do" anything, but out of an abundance of caution:
            mem::drop(datum_slice);
            // Now there shouldn't be any other references to that slice, so this can be deallocated:
            unsafe { pg_sys::pfree(data.as_ptr().cast()) };

            // Detoasting the varlena may have allocated: the toasted varlena cloned as a detoasted ArrayType
            // Checking for pointer equivalence is the only way we can truly tell
            let raw = raw.take().map(|r| r.into_ptr());
            if let Some(raw) = raw {
                // SAFETY: if pgx detoasted a clone of this varlena, pfree the clone
                if raw.cast() != self.ptr {
                    unsafe { pg_sys::pfree(raw.as_ptr().cast()) }
                }
            }
        }
    }
}

#[deny(unsafe_op_in_unsafe_fn)]
impl<'a, T: FromDatum> Array<'a, T> {
    /// # Safety
    ///
    /// This function requires that the RawArray was obtained in a properly-constructed form
    /// (probably from Postgres).
    unsafe fn deconstruct_from(
        ptr: NonNull<pg_sys::varlena>,
        raw: RawArray,
        elem_layout: Layout,
    ) -> Array<'a, T> {
        let oid = raw.oid();
        let len = raw.len();
        let array = raw.into_ptr().as_ptr();

        match (elem_layout.matches::<T>(), elem_layout.pass) {
            (Some(1 | 2 | 4), PassBy::Value) => {
                let mut raw = unsafe { RawArray::from_ptr(NonNull::new_unchecked(array)) };

                let null_slice = raw
                    .nulls_bitslice()
                    .map(|nonnull| NullKind::Bits(unsafe { &*nonnull.as_ptr() }))
                    .unwrap_or(NullKind::Strict(len));
                let elems = raw.data::<T>().cast();

                Array {
                    ptr,
                    raw: Some(raw),
                    nelems: len,
                    datum_palloc: None,
                    datum_slice: None,
                    elems_ptr: Some(elems),
                    null_slice,
                    elem_layout,
                    _marker: PhantomData,
                }
            }
            (_, _) => {
                // outvals for deconstruct_array
                let mut elements = ptr::null_mut();
                let mut nulls = ptr::null_mut();
                let mut nelems = 0;

                /*
                FIXME(jubilee): This way of getting array buffers causes problems for any Drop impl,
                and clashes with assumptions of Array being a "zero-copy", lifetime-bound array,
                some of which are implicitly embedded in other methods (e.g. Array::over).
                It also risks leaking memory, as deconstruct_array calls palloc.

                SAFETY: We have already asserted the validity of the RawArray, so
                this only makes mistakes if we mix things up and pass Postgres the wrong data.
                */
                unsafe {
                    pg_sys::deconstruct_array(
                        array,
                        oid,
                        elem_layout.size.as_typlen().into(),
                        matches!(elem_layout.pass, PassBy::Value),
                        elem_layout.align.as_typalign(),
                        &mut elements,
                        &mut nulls,
                        &mut nelems,
                    )
                };

                let nelems = nelems as usize;

                // Check our RawArray len impl for correctness.
                assert_eq!(nelems, len);
                let mut raw = unsafe { RawArray::from_ptr(NonNull::new_unchecked(array)) };

                let null_slice = raw
                    .nulls_bitslice()
                    .map(|nonnull| NullKind::Bits(unsafe { &*nonnull.as_ptr() }))
                    .unwrap_or(NullKind::Strict(nelems));

                // The array was just deconstructed, which allocates twice: effectively [Datum] and [bool].
                // But pgx doesn't actually need [bool] if NullKind's handling of BitSlices is correct.
                // So, assert correctness of the NullKind implementation and cleanup.
                // SAFETY: The pointer we got should be correctly constructed for slice validity.
                let pallocd_null_slice = unsafe { slice::from_raw_parts(nulls, nelems) };
                #[cfg(debug_assertions)]
                for i in 0..nelems {
                    assert_eq!(null_slice.get(i).unwrap(), pallocd_null_slice[i]);
                }

                // Throw away the slice we made.
                mem::drop(pallocd_null_slice);
                // SAFETY: We made it, we can break it. Or Postgres can, at least.
                unsafe { pg_sys::pfree(nulls.cast()) };

                Array {
                    ptr,
                    raw: Some(raw),
                    nelems,
                    datum_palloc: NonNull::new(elements),
                    datum_slice: /* SAFETY: &[Datum] from palloc'd [Datum] */ Some(unsafe { slice::from_raw_parts(elements, nelems) }),
                    elems_ptr: None,
                    null_slice,
                    elem_layout,
                    _marker: PhantomData,
                }
            }
        }
    }

    pub fn into_array_type(mut self) -> *const pg_sys::ArrayType {
        let ptr = mem::take(&mut self.raw).map(|raw| raw.into_ptr().as_ptr() as _);
        mem::forget(self);
        ptr.unwrap_or(ptr::null())
    }

    // # Panics
    //
    // Panics if it detects the slightest misalignment between types,
    // or if a valid slice contains nulls, which may be uninit data.
    #[deprecated(
        since = "0.5.0",
        note = "this function cannot be safe and is not generically sound\n\
        even `unsafe fn as_slice(&self) -> &[T]` is not sound for all `&[T]`\n\
        if you are sure your usage is sound, consider RawArray"
    )]
    pub fn as_slice(&self) -> &[T] {
        const DATUM_SIZE: usize = mem::size_of::<pg_sys::Datum>();
        if self.null_slice.any() {
            panic!("null detected: can't expose potentially uninit data as a slice!")
        }
        match (self.elem_layout.matches::<T>(), self.raw.as_ref()) {
            // SAFETY: Rust slice layout matches Postgres data layout and this array is "owned"
            (Some(1 | 2 | 4 | DATUM_SIZE), Some(raw)) => unsafe {
                raw.assume_init_data_slice::<T>()
            },
            (_, _) => panic!("no correctly-sized slice exists"),
        }
    }

    /// Return an Iterator of Option<T> over the contained Datums.
    pub fn iter(&self) -> ArrayIterator<'_, T> {
        ArrayIterator { array: self, curr: 0 }
    }

    /// Return an Iterator of the contained Datums (converted to Rust types).
    ///
    /// This function will panic when called if the array contains any SQL NULL values.
    pub fn iter_deny_null(&self) -> ArrayTypedIterator<'_, T> {
        if let Some(at) = &self.raw {
            // SAFETY: if Some, then the ArrayType is from Postgres
            if unsafe { at.any_nulls() } {
                panic!("array contains NULL");
            }
        } else {
            panic!("array is NULL");
        };

        ArrayTypedIterator { array: self, curr: 0 }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.nelems
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.nelems == 0
    }

    /// # Panics
    /// Panics if you attempt to do random access on a pass-by-value array
    #[allow(clippy::option_option)]
    #[inline]
    pub fn get(&self, i: usize) -> Option<Option<T>> {
        self.null_slice.get(i).map(|is_null| {
            if let Some(datums) = self.datum_slice {
                unsafe {
                    T::from_polymorphic_datum(
                        datums[i],
                        is_null,
                        self.raw.as_ref().map(|r| r.oid())?,
                    )
                }
            } else if let Some(elems) = self.elems_ptr {
                // SAFETY: barely. we're getting around the trait bounds not being better
                // by just doing something wildly unsafe instead: reading a raw addr.
                // the only reason this is okay is because we make elems_ptr = Some(ptr)
                // if and only if we believe we can get away with this
                (!is_null).then(|| unsafe { elems.as_ptr().add(i).read() })
            } else {
                panic!("poorly constructed array type!");
            }
        })
    }
}

pub struct VariadicArray<'a, T: FromDatum>(Array<'a, T>);

impl<'a, T: FromDatum + serde::Serialize> serde::Serialize for VariadicArray<'a, T> {
    fn serialize<S>(&self, serializer: S) -> Result<<S as Serializer>::Ok, <S as Serializer>::Error>
    where
        S: Serializer,
    {
        serializer.collect_seq(self.0.iter())
    }
}

impl<'a, T: FromDatum> VariadicArray<'a, T> {
    pub fn into_array_type(self) -> *const pg_sys::ArrayType {
        self.0.into_array_type()
    }

    // # Panics
    //
    // Panics if it detects the slightest misalignment between types,
    // or if a valid slice contains nulls, which may be uninit data.
    #[deprecated(
        since = "0.5.0",
        note = "this function cannot be safe and is not generically sound\n\
        even `unsafe fn as_slice(&self) -> &[T]` is not sound for all `&[T]`\n\
        if you are sure your usage is sound, consider RawArray"
    )]
    #[allow(deprecated)]
    pub fn as_slice(&self) -> &[T] {
        self.0.as_slice()
    }

    /// Return an Iterator of Option<T> over the contained Datums.
    pub fn iter(&self) -> ArrayIterator<'_, T> {
        self.0.iter()
    }

    /// Return an Iterator of the contained Datums (converted to Rust types).
    ///
    /// This function will panic when called if the array contains any SQL NULL values.
    pub fn iter_deny_null(&self) -> ArrayTypedIterator<'_, T> {
        self.0.iter_deny_null()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[allow(clippy::option_option)]
    #[inline]
    pub fn get(&self, i: usize) -> Option<Option<T>> {
        self.0.get(i)
    }
}

pub struct ArrayTypedIterator<'a, T: 'a + FromDatum> {
    array: &'a Array<'a, T>,
    curr: usize,
}

impl<'a, T: FromDatum> Iterator for ArrayTypedIterator<'a, T> {
    type Item = T;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.curr >= self.array.nelems {
            None
        } else {
            let element = self
                .array
                .get(self.curr)
                .expect("array index out of bounds")
                .expect("array element was unexpectedly NULL during iteration");
            self.curr += 1;
            Some(element)
        }
    }
}

impl<'a, T: FromDatum + serde::Serialize> serde::Serialize for ArrayTypedIterator<'a, T> {
    fn serialize<S>(&self, serializer: S) -> Result<<S as Serializer>::Ok, <S as Serializer>::Error>
    where
        S: Serializer,
    {
        serializer.collect_seq(self.array.iter())
    }
}

pub struct ArrayIterator<'a, T: 'a + FromDatum> {
    array: &'a Array<'a, T>,
    curr: usize,
}

impl<'a, T: FromDatum> Iterator for ArrayIterator<'a, T> {
    type Item = Option<T>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.curr >= self.array.nelems {
            None
        } else {
            let element = self.array.get(self.curr).unwrap();
            self.curr += 1;
            Some(element)
        }
    }
}

pub struct ArrayIntoIterator<'a, T: FromDatum> {
    array: Array<'a, T>,
    curr: usize,
}

impl<'a, T: FromDatum> IntoIterator for Array<'a, T> {
    type Item = Option<T>;
    type IntoIter = ArrayIntoIterator<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        ArrayIntoIterator { array: self, curr: 0 }
    }
}

impl<'a, T: FromDatum> IntoIterator for VariadicArray<'a, T> {
    type Item = Option<T>;
    type IntoIter = ArrayIntoIterator<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        ArrayIntoIterator { array: self.0, curr: 0 }
    }
}

impl<'a, T: FromDatum> Iterator for ArrayIntoIterator<'a, T> {
    type Item = Option<T>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.curr >= self.array.nelems {
            None
        } else {
            let element = self.array.get(self.curr).unwrap();
            self.curr += 1;
            Some(element)
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(self.array.nelems))
    }

    fn count(self) -> usize
    where
        Self: Sized,
    {
        self.array.nelems
    }

    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        self.array.get(n)
    }
}

impl<'a, T: FromDatum> FromDatum for VariadicArray<'a, T> {
    #[inline]
    unsafe fn from_polymorphic_datum(
        datum: pg_sys::Datum,
        is_null: bool,
        oid: pg_sys::Oid,
    ) -> Option<VariadicArray<'a, T>> {
        Array::from_polymorphic_datum(datum, is_null, oid).map(Self)
    }
}

impl<'a, T: FromDatum> FromDatum for Array<'a, T> {
    #[inline]
    unsafe fn from_polymorphic_datum(
        datum: pg_sys::Datum,
        is_null: bool,
        _typoid: u32,
    ) -> Option<Array<'a, T>> {
        if is_null {
            None
        } else {
            let ptr = NonNull::new(datum.cast_mut_ptr())?;
            let array = pg_sys::pg_detoast_datum(datum.cast_mut_ptr()) as *mut pg_sys::ArrayType;
            let raw =
                RawArray::from_ptr(NonNull::new(array).expect("detoast returned null ArrayType*"));
            let oid = raw.oid();
            let layout = Layout::lookup_oid(oid);

            Some(Array::deconstruct_from(ptr, raw, layout))
        }
    }
}

impl<T: FromDatum> FromDatum for Vec<T> {
    #[inline]
    unsafe fn from_polymorphic_datum(
        datum: pg_sys::Datum,
        is_null: bool,
        typoid: pg_sys::Oid,
    ) -> Option<Vec<T>> {
        if is_null {
            None
        } else {
            let array = Array::<T>::from_polymorphic_datum(datum, is_null, typoid).unwrap();
            let mut v = Vec::with_capacity(array.len());

            for element in array.iter() {
                v.push(element.expect("array element was NULL"))
            }
            Some(v)
        }
    }
}

impl<T: FromDatum> FromDatum for Vec<Option<T>> {
    #[inline]
    unsafe fn from_polymorphic_datum(
        datum: pg_sys::Datum,
        is_null: bool,
        typoid: pg_sys::Oid,
    ) -> Option<Vec<Option<T>>> {
        if is_null || datum.is_null() {
            None
        } else {
            let array = Array::<T>::from_polymorphic_datum(datum, is_null, typoid).unwrap();
            let mut v = Vec::with_capacity(array.len());

            for element in array.iter() {
                v.push(element)
            }
            Some(v)
        }
    }
}

impl<T> IntoDatum for Vec<T>
where
    T: IntoDatum,
{
    fn into_datum(self) -> Option<pg_sys::Datum> {
        let mut state = unsafe {
            pg_sys::initArrayResult(
                T::type_oid(),
                PgMemoryContexts::CurrentMemoryContext.value(),
                false,
            )
        };
        for s in self {
            let datum = s.into_datum();
            let isnull = datum.is_none();

            unsafe {
                state = pg_sys::accumArrayResult(
                    state,
                    datum.unwrap_or(0.into()),
                    isnull,
                    T::type_oid(),
                    PgMemoryContexts::CurrentMemoryContext.value(),
                );
            }
        }

        if state.is_null() {
            // shoudln't happen
            None
        } else {
            Some(unsafe {
                pg_sys::makeArrayResult(state, PgMemoryContexts::CurrentMemoryContext.value())
            })
        }
    }

    fn type_oid() -> u32 {
        unsafe { pg_sys::get_array_type(T::type_oid()) }
    }

    #[inline]
    fn is_compatible_with(other: pg_sys::Oid) -> bool {
        Self::type_oid() == other || other == unsafe { pg_sys::get_array_type(T::type_oid()) }
    }
}

impl<'a, T> IntoDatum for &'a [T]
where
    T: IntoDatum + Copy + 'a,
{
    fn into_datum(self) -> Option<pg_sys::Datum> {
        let mut state = unsafe {
            pg_sys::initArrayResult(
                T::type_oid(),
                PgMemoryContexts::CurrentMemoryContext.value(),
                false,
            )
        };
        for s in self {
            let datum = s.into_datum();
            let isnull = datum.is_none();

            unsafe {
                state = pg_sys::accumArrayResult(
                    state,
                    datum.unwrap_or(0.into()),
                    isnull,
                    T::type_oid(),
                    PgMemoryContexts::CurrentMemoryContext.value(),
                );
            }
        }

        if state.is_null() {
            // shoudln't happen
            None
        } else {
            Some(unsafe {
                pg_sys::makeArrayResult(state, PgMemoryContexts::CurrentMemoryContext.value())
            })
        }
    }

    fn type_oid() -> u32 {
        unsafe { pg_sys::get_array_type(T::type_oid()) }
    }

    #[inline]
    fn is_compatible_with(other: pg_sys::Oid) -> bool {
        Self::type_oid() == other || other == unsafe { pg_sys::get_array_type(T::type_oid()) }
    }
}

unsafe impl<'a, T> SqlTranslatable for Array<'a, T>
where
    T: SqlTranslatable + FromDatum,
{
    fn argument_sql() -> Result<SqlMapping, ArgumentError> {
        match T::argument_sql()? {
            SqlMapping::As(sql) => Ok(SqlMapping::As(format!("{sql}[]"))),
            SqlMapping::Skip => Err(ArgumentError::SkipInArray),
            SqlMapping::Composite { .. } => Ok(SqlMapping::Composite { array_brackets: true }),
            SqlMapping::Source { .. } => Ok(SqlMapping::Source { array_brackets: true }),
        }
    }

    fn return_sql() -> Result<Returns, ReturnsError> {
        match T::return_sql()? {
            Returns::One(SqlMapping::As(sql)) => {
                Ok(Returns::One(SqlMapping::As(format!("{sql}[]"))))
            }
            Returns::One(SqlMapping::Composite { array_brackets: _ }) => {
                Ok(Returns::One(SqlMapping::Composite { array_brackets: true }))
            }
            Returns::One(SqlMapping::Source { array_brackets: _ }) => {
                Ok(Returns::One(SqlMapping::Source { array_brackets: true }))
            }
            Returns::One(SqlMapping::Skip) => Err(ReturnsError::SkipInArray),
            Returns::SetOf(_) => Err(ReturnsError::SetOfInArray),
            Returns::Table(_) => Err(ReturnsError::TableInArray),
        }
    }
}

unsafe impl<'a, T> SqlTranslatable for VariadicArray<'a, T>
where
    T: SqlTranslatable + FromDatum,
{
    fn argument_sql() -> Result<SqlMapping, ArgumentError> {
        match T::argument_sql()? {
            SqlMapping::As(sql) => Ok(SqlMapping::As(format!("{sql}[]"))),
            SqlMapping::Skip => Err(ArgumentError::SkipInArray),
            SqlMapping::Composite { .. } => Ok(SqlMapping::Composite { array_brackets: true }),
            SqlMapping::Source { .. } => Ok(SqlMapping::Source { array_brackets: true }),
        }
    }

    fn return_sql() -> Result<Returns, ReturnsError> {
        match T::return_sql()? {
            Returns::One(SqlMapping::As(sql)) => {
                Ok(Returns::One(SqlMapping::As(format!("{sql}[]"))))
            }
            Returns::One(SqlMapping::Composite { array_brackets: _ }) => {
                Ok(Returns::One(SqlMapping::Composite { array_brackets: true }))
            }
            Returns::One(SqlMapping::Source { array_brackets: _ }) => {
                Ok(Returns::One(SqlMapping::Source { array_brackets: true }))
            }
            Returns::One(SqlMapping::Skip) => Err(ReturnsError::SkipInArray),
            Returns::SetOf(_) => Err(ReturnsError::SetOfInArray),
            Returns::Table(_) => Err(ReturnsError::TableInArray),
        }
    }

    fn variadic() -> bool {
        true
    }
}
