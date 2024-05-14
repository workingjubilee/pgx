//LICENSE Portions Copyright 2019-2021 ZomboDB, LLC.
//LICENSE
//LICENSE Portions Copyright 2021-2023 Technology Concepts & Design, Inc.
//LICENSE
//LICENSE Portions Copyright 2023-2023 PgCentral Foundation, Inc. <contact@pgcentral.org>
//LICENSE
//LICENSE All rights reserved.
//LICENSE
//LICENSE Use of this source code is governed by the MIT license that can be found in the LICENSE file.
#![doc(hidden)]
#![allow(unused)]
//! Helper implementations for returning sets and tables from `#[pg_extern]`-style functions
use std::ffi::{CStr, CString};
use std::marker::PhantomData;
use std::mem::{self, ManuallyDrop};

use crate::heap_tuple::PgHeapTuple;
use crate::iter::{SetOfIterator, TableIterator};
use crate::ptr::PointerExt;
use crate::{
    nonstatic_typeid, pg_return_null, pg_sys, srf_is_first_call, srf_return_done, srf_return_next,
    AnyNumeric, Date, Inet, Internal, Interval, IntoDatum, IntoHeapTuple, Json, PgBox,
    PgMemoryContexts, PgVarlena, Time, TimeWithTimeZone, Timestamp, TimestampWithTimeZone, Uuid,
};
use core::ops::ControlFlow;
use core::ptr;

/// Unboxing for arguments
///
/// This bound is necessary to distinguish things which can be passed into `#[pg_extern] fn`.
/// It is strictly a mistake to use the BorrowDatum/UnboxDatum/DetoastDatum traits for this bound!
/// PGRX allows "phantom arguments" which are not actually present in the C function, and are also
/// omitted in the SQL, but are passed to the Rust function anyways.
pub trait UnboxArg {
    /// indicates min/max number of args that may be consumed if statically known
    fn arg_width(fcinfo: pg_sys::FunctionCallInfo) -> Option<(usize, usize)> {
        todo!()
    }

    /// try to unbox the next argument
    ///
    /// should play into a quasi-iterator somehow?
    fn try_unbox(fcinfo: pg_sys::FunctionCallInfo, current: usize) -> ControlFlow<Self, ()>
    where
        Self: Sized,
    {
        todo!()
    }
}

/// Boxing for return values
///
/// This bound is necessary to distinguish things which can be passed in/out of `#[pg_extern] fn`.
/// It is strictly a mistake to use IntoDatum or any derived traits for this bound!
/// PGRX allows complex return values which are nonsensical outside of function boundaries,
/// e.g. for implementing the value-per-call convention for set-returning functions.
pub unsafe trait BoxRet: Sized {
    /// The actual type returned from the call
    type CallRet: Sized;

    /// check the fcinfo state, initialize if necessary, and pick calling the wrapped fn or restoring Self
    ///
    /// the implementer must pick the correct memory context for the wrapped fn's allocations
    /// # safety
    /// must be called with a valid fcinfo
    unsafe fn prepare_call(fcinfo: pg_sys::FunctionCallInfo) -> CallCx {
        CallCx::WrappedFn(unsafe { pg_sys::CurrentMemoryContext })
    }

    /// answer what kind and how many returns happen from this type
    ///
    /// must be overridden if `Self != Self::CallRet`
    fn into_ret(self) -> Ret<Self>
    where
        Self: Sized;
    // morally I should be allowed to supply a default impl >:(
    /* this default impl would work, but at what cost?
    {
        if nonstatic_typeid::<Self>() == nonstatic_typeid::<Self::CallRet>() {
            // SAFETY: We materialize a copy, then evaporate the original without dropping it,
            // but only when we know that the original Self is the same type as Self::CallRet!
            unsafe {
                let ret = Ret::Once(::core::mem::transmute_copy(&self));
                ::core::mem::forget(self);
                ret
            }
        } else {
            panic!("`BoxRet::into_ret` must be overridden for {}", ::core::any::type_name::<Self>())
        }
    }
    */

    /// box the return value
    /// # Safety
    /// must be called with a valid fcinfo
    unsafe fn box_return(fcinfo: pg_sys::FunctionCallInfo, ret: Ret<Self>) -> pg_sys::Datum;

    /// for multi-call types, how to init them in the multi-call context, for all others: panic
    ///
    /// for all others: panic
    /// # Safety
    /// must be called with a valid fcinfo
    unsafe fn into_context(self, fcinfo: pg_sys::FunctionCallInfo) {
        unimplemented!()
    }

    /// for multi-call types, how to restore them from the multi-call context
    ///
    /// for all others: panic
    /// # Safety
    /// must be called with a valid fcinfo
    unsafe fn ret_from_context(fcinfo: pg_sys::FunctionCallInfo) -> Ret<Self> {
        unimplemented!()
    }

    /// must be called with a valid fcinfo
    unsafe fn finish_call(_fcinfo: pg_sys::FunctionCallInfo) {}
}

// pub unsafe fn handle_ret<T: BoxRet>(
//     fcinfo: pg_sys::FunctionCallInfo,
//     ret: Ret<T>,
// ) -> pg_sys::Datum {
//     match ret {
//         // Single-call fn or value-per-call iteration
//         Ret::Once(value) => <T as BoxRet>::box_return(fcinfo, value),
//         // Value-per-call first-time
//         Ret::Many(iter, value) => {
//             iter.into_context(fcinfo);
//             <T as BoxRet>::box_return(fcinfo, value)
//         }
//         // Value-per-call last-time
//         Ret::Zero => {
//             <T as BoxRet>::finish_call(fcinfo);
//             unsafe { pg_return_null(fcinfo) }
//         }
//     }
// }

pub enum CallCx {
    RestoreCx,
    WrappedFn(pg_sys::MemoryContext),
}

fn prepare_value_per_call_srf(fcinfo: pg_sys::FunctionCallInfo) -> CallCx {
    unsafe {
        if srf_is_first_call(fcinfo) {
            let fn_call_cx = pg_sys::init_MultiFuncCall(fcinfo);
            CallCx::WrappedFn((*fn_call_cx).multi_call_memory_ctx)
        } else {
            CallCx::RestoreCx
        }
    }
}

unsafe impl<'a, T> BoxRet for SetOfIterator<'a, T>
where
    T: BoxRet,
{
    type CallRet = <Self as Iterator>::Item;
    unsafe fn prepare_call(fcinfo: pg_sys::FunctionCallInfo) -> CallCx {
        prepare_value_per_call_srf(fcinfo)
    }

    fn into_ret(self) -> Ret<Self>
    where
        Self: Sized,
    {
        let mut iter = self;
        let ret = iter.next();
        match ret {
            None => Ret::Zero,
            Some(value) => Ret::Many(iter, value),
        }
    }

    unsafe fn box_return(fcinfo: pg_sys::FunctionCallInfo, ret: Ret<Self>) -> pg_sys::Datum {
        let fcx = deref_fcx(fcinfo);
        let value = match ret {
            Ret::Zero => return empty_srf(fcinfo),
            Ret::Once(value) => value,
            Ret::Many(iter, value) => {
                iter.into_context(fcinfo);
                value
            }
        };

        unsafe {
            let fcx = deref_fcx(fcinfo);
            srf_return_next(fcinfo, fcx);
            T::box_return(fcinfo, value.into_ret())
        }
    }

    unsafe fn into_context(self, fcinfo: pg_sys::FunctionCallInfo) {
        let fcx = deref_fcx(fcinfo);
        unsafe {
            let ptr = srf_memcx(fcx).leak_and_drop_on_delete(self);
            // it's the first call so we need to finish setting up fcx
            (*fcx).user_fctx = ptr.cast();
        }
    }

    unsafe fn ret_from_context(fcinfo: pg_sys::FunctionCallInfo) -> Ret<Self> {
        let fcx = deref_fcx(fcinfo);
        // SAFETY: fcx.user_fctx was set earlier, immediately before or in a prior call
        let mut iter = &mut *(*fcx).user_fctx.cast::<TableIterator<T>>();
        match iter.next() {
            None => Ret::Zero,
            Some(value) => Ret::Once(value),
        }
    }

    unsafe fn finish_call(fcinfo: pg_sys::FunctionCallInfo) {
        let fcx = deref_fcx(fcinfo);
        unsafe { srf_return_done(fcinfo, fcx) }
    }
}

pub enum Ret<T: BoxRet> {
    Zero,
    Once(T::CallRet),
    Many(T, T::CallRet),
}

impl<'a, T: IntoDatum> SetOfIterator<'a, T> {
    #[doc(hidden)]
    pub unsafe fn srf_next(
        fcinfo: pg_sys::FunctionCallInfo,
        wrapped_fn: impl FnOnce() -> Option<SetOfIterator<'a, T>>,
    ) -> pg_sys::Datum {
        if fcx_needs_setup(fcinfo) {
            let fcx = deref_fcx(fcinfo);
            // first off, ask the user's function to do the needful and return Option<SetOfIterator<T>>
            let setof_iterator = srf_memcx(fcx).switch_to(|_| wrapped_fn());
            if let ControlFlow::Break(datum) = finish_srf_init(setof_iterator, fcinfo) {
                return datum;
            }
        }

        let fcx = deref_fcx(fcinfo);
        // SAFETY: fcx.user_fctx was set earlier, immediately before or in a prior call
        let setof_iterator = &mut *(*fcx).user_fctx.cast::<SetOfIterator<T>>();

        match setof_iterator.next() {
            Some(datum) => {
                srf_return_next(fcinfo, fcx);
                datum.into_datum().unwrap_or_else(|| pg_return_null(fcinfo))
            }
            None => empty_srf(fcinfo),
        }
    }
}

unsafe impl<'a, T> BoxRet for TableIterator<'a, T>
where
    T: BoxRet,
{
    type CallRet = <Self as Iterator>::Item;

    unsafe fn prepare_call(fcinfo: pg_sys::FunctionCallInfo) -> CallCx {
        prepare_value_per_call_srf(fcinfo)
    }

    fn into_ret(self) -> Ret<Self>
    where
        Self: Sized,
    {
        let mut iter = self;
        let ret = iter.next();
        match ret {
            None => Ret::Zero,
            Some(value) => Ret::Many(iter, value),
        }
    }

    unsafe fn box_return(fcinfo: pg_sys::FunctionCallInfo, ret: Ret<Self>) -> pg_sys::Datum {
        let value = match ret {
            Ret::Zero => return empty_srf(fcinfo),
            Ret::Once(value) => value,
            Ret::Many(iter, value) => {
                iter.into_context(fcinfo);
                value
            }
        };

        unsafe {
            let fcx = deref_fcx(fcinfo);
            srf_return_next(fcinfo, fcx);
            T::box_return(fcinfo, value.into_ret())
        }
    }

    unsafe fn into_context(self, fcinfo: pg_sys::FunctionCallInfo) {
        // FIXME: this is assigned here but used in the tuple impl?
        let fcx = deref_fcx(fcinfo);
        unsafe {
            let ptr = srf_memcx(fcx).switch_to(move |mcx| {
                let mut tupdesc = ptr::null_mut();
                let mut oid = pg_sys::Oid::default();
                let ty_class = pg_sys::get_call_result_type(fcinfo, &mut oid, &mut tupdesc);
                if tupdesc.is_non_null() {
                    pg_sys::BlessTupleDesc(tupdesc);
                }
                (*fcx).tuple_desc = tupdesc;
                mcx.leak_and_drop_on_delete(self)
            });
            // it's the first call so we need to finish setting up fcx
            (*fcx).user_fctx = ptr.cast();
        }
    }

    unsafe fn ret_from_context(fcinfo: pg_sys::FunctionCallInfo) -> Ret<Self> {
        let fcx = deref_fcx(fcinfo);
        // SAFETY: fcx.user_fctx was set earlier, immediately before or in a prior call
        let mut iter = &mut *(*fcx).user_fctx.cast::<TableIterator<T>>();
        match iter.next() {
            None => Ret::Zero,
            Some(value) => Ret::Once(value),
        }
    }

    unsafe fn finish_call(fcinfo: pg_sys::FunctionCallInfo) {
        let fcx = deref_fcx(fcinfo);
        unsafe { srf_return_done(fcinfo, fcx) }
    }
}

impl<'a, T: IntoHeapTuple> TableIterator<'a, T> {
    #[doc(hidden)]
    pub unsafe fn srf_next(
        fcinfo: pg_sys::FunctionCallInfo,
        wrapped_fn: impl FnOnce() -> Option<TableIterator<'a, T>>,
    ) -> pg_sys::Datum {
        if fcx_needs_setup(fcinfo) {
            let fcx = deref_fcx(fcinfo);
            let table_iterator = srf_memcx(fcx).switch_to(|_| {
                // first off, ask the user's function to do the needful and return Option<TableIterator<T>>
                let table_iterator = wrapped_fn();

                // Build a tuple descriptor for our result type
                let mut tupdesc = ptr::null_mut();
                let ty_class = pg_sys::get_call_result_type(fcinfo, ptr::null_mut(), &mut tupdesc);
                if ty_class != pg_sys::TypeFuncClass_TYPEFUNC_COMPOSITE {
                    pg_sys::error!("return type must be a row type");
                }
                pg_sys::BlessTupleDesc(tupdesc);
                (*fcx).tuple_desc = tupdesc;

                table_iterator
            });

            if let ControlFlow::Break(datum) = finish_srf_init(table_iterator, fcinfo) {
                return datum;
            }
        }

        let fcx = deref_fcx(fcinfo);
        // SAFETY: fcx.user_fctx was set earlier, immediately before or in a prior call
        let table_iterator = &mut *(*fcx).user_fctx.cast::<TableIterator<T>>();

        match table_iterator.next() {
            Some(tuple) => {
                let heap_tuple = tuple.into_heap_tuple((*fcx).tuple_desc);
                srf_return_next(fcinfo, fcx);
                pg_sys::HeapTupleHeaderGetDatum((*heap_tuple).t_data)
            }
            None => empty_srf(fcinfo),
        }
    }
}

fn fcx_needs_setup(fcinfo: pg_sys::FunctionCallInfo) -> bool {
    let need = unsafe { srf_is_first_call(fcinfo) };
    if need {
        unsafe { pg_sys::init_MultiFuncCall(fcinfo) };
    }
    need
}

pub(crate) fn empty_srf(fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum {
    unsafe {
        let fcx = deref_fcx(fcinfo);
        srf_return_done(fcinfo, fcx);
        pg_return_null(fcinfo)
    }
}

/// "per_MultiFuncCall" but no FFI cost
pub(crate) fn deref_fcx(fcinfo: pg_sys::FunctionCallInfo) -> *mut pg_sys::FuncCallContext {
    unsafe { (*(*fcinfo).flinfo).fn_extra.cast() }
}

pub(crate) fn srf_memcx(fcx: *mut pg_sys::FuncCallContext) -> PgMemoryContexts {
    unsafe { PgMemoryContexts::For((*fcx).multi_call_memory_ctx) }
}

fn finish_srf_init<T>(
    arg: Option<T>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> ControlFlow<pg_sys::Datum, ()> {
    match arg {
        // nothing to iterate?
        None => ControlFlow::Break(empty_srf(fcinfo)),
        // must be saved for the next call by leaking it into the multi-call memory context
        Some(value) => {
            let fcx = deref_fcx(fcinfo);
            unsafe {
                let ptr = srf_memcx(fcx).leak_and_drop_on_delete(value);
                // it's the first call so we need to finish setting up fcx
                (*fcx).user_fctx = ptr.cast();
            }
            ControlFlow::Continue(())
        }
    }
}

unsafe impl<T> BoxRet for Option<T>
where
    T: BoxRet,
    T::CallRet: BoxRet,
{
    type CallRet = T::CallRet;

    unsafe fn prepare_call(fcinfo: pg_sys::FunctionCallInfo) -> CallCx {
        T::prepare_call(fcinfo)
    }

    fn into_ret(self) -> Ret<Self>
    where
        Self: Sized,
    {
        match self {
            None => Ret::Zero,
            Some(value) => match value.into_ret() {
                Ret::Many(iter, value) => Ret::Many(Some(iter), value),
                Ret::Once(value) => Ret::Once(value),
                Ret::Zero => Ret::Zero,
            },
        }
    }

    unsafe fn box_return(fcinfo: pg_sys::FunctionCallInfo, ret: Ret<Self>) -> pg_sys::Datum {
        let inner = match ret {
            Ret::Zero => Ret::Zero,
            Ret::Once(value) => Ret::Once(value),
            Ret::Many(Some(iter), value) => Ret::Many(iter, value),
            Ret::Many(None, _) => Ret::Zero,
        };

        T::box_return(fcinfo, inner)
    }

    unsafe fn into_context(self, fcinfo: pg_sys::FunctionCallInfo) {
        match self {
            None => (),
            Some(value) => value.into_context(fcinfo),
        }
    }

    unsafe fn ret_from_context(fcinfo: pg_sys::FunctionCallInfo) -> Ret<Self> {
        match T::ret_from_context(fcinfo) {
            Ret::Many(iter, value) => Ret::Many(Some(iter), value),
            Ret::Once(value) => Ret::Once(value),
            Ret::Zero => Ret::Zero,
        }
    }

    unsafe fn finish_call(fcinfo: pg_sys::FunctionCallInfo) {
        unsafe { T::finish_call(fcinfo) }
    }
}

unsafe impl<T, E> BoxRet for Result<T, E>
where
    T: BoxRet,
    T::CallRet: BoxRet,
    E: core::any::Any + core::fmt::Display,
{
    type CallRet = T::CallRet;

    unsafe fn prepare_call(fcinfo: pg_sys::FunctionCallInfo) -> CallCx {
        T::prepare_call(fcinfo)
    }

    fn into_ret(self) -> Ret<Self>
    where
        Self: Sized,
    {
        let value = pg_sys::panic::ErrorReportable::unwrap_or_report(self);
        match T::into_ret(value) {
            Ret::Zero => Ret::Zero,
            Ret::Once(value) => Ret::Once(value),
            Ret::Many(iter, value) => Ret::Many(Ok(iter), value),
        }
    }

    unsafe fn box_return(fcinfo: pg_sys::FunctionCallInfo, ret: Ret<Self>) -> pg_sys::Datum {
        let ret = match ret {
            Ret::Zero => Ret::Zero,
            Ret::Once(value) => Ret::Once(value),
            Ret::Many(iter, value) => {
                let iter = pg_sys::panic::ErrorReportable::unwrap_or_report(iter);
                Ret::Many(iter, value)
            }
        };

        T::box_return(fcinfo, ret)
    }

    unsafe fn into_context(self, fcinfo: pg_sys::FunctionCallInfo) {
        match self {
            Err(_) => (),
            Ok(value) => value.into_context(fcinfo),
        }
    }

    unsafe fn ret_from_context(fcinfo: pg_sys::FunctionCallInfo) -> Ret<Self> {
        match T::ret_from_context(fcinfo) {
            Ret::Many(iter, value) => Ret::Many(Ok(iter), value),
            Ret::Once(value) => Ret::Once(value),
            Ret::Zero => Ret::Zero,
        }
    }

    unsafe fn finish_call(fcinfo: pg_sys::FunctionCallInfo) {
        T::finish_call(fcinfo)
    }
}

macro_rules! impl_boxret_for_primitives {
    ($($scalar:ty),*) => {
        $(
        unsafe impl BoxRet for $scalar {
            type CallRet = Self;
            fn into_ret(self) -> Ret<Self>
            where
                Self: Sized,
            {
                Ret::Once(self)
            }

            unsafe fn box_return(fcinfo: pg_sys::FunctionCallInfo, ret: Ret<Self>) -> pg_sys::Datum {
                match ret {
                    Ret::Zero => unsafe { pg_return_null(fcinfo) },
                    Ret::Once(value) => $crate::pg_sys::Datum::from(value),
                    Ret::Many(_, _) => unreachable!()
                }
            }
        }
        )*
    }
}

impl_boxret_for_primitives! {
    i8, i16, i32, i64, bool
}

unsafe impl BoxRet for () {
    type CallRet = Self;
    fn into_ret(self) -> Ret<Self>
    where
        Self: Sized,
    {
        Ret::Zero
    }

    unsafe fn box_return(fcinfo: pg_sys::FunctionCallInfo, ret: Ret<Self>) -> pg_sys::Datum {
        pg_sys::Datum::from(0)
    }
}

unsafe impl BoxRet for f32 {
    type CallRet = Self;
    fn into_ret(self) -> Ret<Self>
    where
        Self: Sized,
    {
        Ret::Once(self)
    }

    unsafe fn box_return(fcinfo: pg_sys::FunctionCallInfo, ret: Ret<Self>) -> pg_sys::Datum {
        match ret {
            Ret::Zero => unsafe { pg_return_null(fcinfo) },
            Ret::Once(value) => pg_sys::Datum::from(value.to_bits()),
            Ret::Many(_, _) => unreachable!(),
        }
    }
}

unsafe impl BoxRet for f64 {
    type CallRet = Self;
    fn into_ret(self) -> Ret<Self>
    where
        Self: Sized,
    {
        Ret::Once(self)
    }

    unsafe fn box_return(fcinfo: pg_sys::FunctionCallInfo, ret: Ret<Self>) -> pg_sys::Datum {
        match ret {
            Ret::Zero => unsafe { pg_return_null(fcinfo) },
            Ret::Once(value) => pg_sys::Datum::from(value.to_bits()),
            Ret::Many(_, _) => unreachable!(),
        }
    }
}

fn boxret_via_into_datum<T: BoxRet<CallRet: IntoDatum>>(
    fcinfo: pg_sys::FunctionCallInfo,
    ret: Ret<T>,
) -> pg_sys::Datum {
    match ret {
        Ret::Zero => unsafe { pg_return_null(fcinfo) },
        Ret::Once(value) => match value.into_datum() {
            None => unsafe { pg_return_null(fcinfo) },
            Some(datum) => datum,
        },
        Ret::Many(_, _) => unreachable!(),
    }
}

unsafe impl<'a> BoxRet for &'a [u8] {
    type CallRet = Self;
    fn into_ret(self) -> Ret<Self>
    where
        Self: Sized,
    {
        Ret::Once(self)
    }

    unsafe fn box_return(fcinfo: pg_sys::FunctionCallInfo, ret: Ret<Self>) -> pg_sys::Datum {
        boxret_via_into_datum(fcinfo, ret)
    }
}

unsafe impl<'a> BoxRet for &'a str {
    type CallRet = Self;
    fn into_ret(self) -> Ret<Self>
    where
        Self: Sized,
    {
        Ret::Once(self)
    }

    unsafe fn box_return(fcinfo: pg_sys::FunctionCallInfo, ret: Ret<Self>) -> pg_sys::Datum {
        boxret_via_into_datum(fcinfo, ret)
    }
}

unsafe impl<'a> BoxRet for &'a CStr {
    type CallRet = Self;
    fn into_ret(self) -> Ret<Self>
    where
        Self: Sized,
    {
        Ret::Once(self)
    }

    unsafe fn box_return(fcinfo: pg_sys::FunctionCallInfo, ret: Ret<Self>) -> pg_sys::Datum {
        boxret_via_into_datum(fcinfo, ret)
    }
}

macro_rules! impl_boxret_via_intodatum {
    ($($boxable:ty),*) => {
        $(
        unsafe impl BoxRet for $boxable {
            type CallRet = Self;
            fn into_ret(self) -> Ret<Self>
            where
                Self: Sized,
            {
                Ret::Once(self)
            }

            unsafe fn box_return(fcinfo: pg_sys::FunctionCallInfo, ret: Ret<Self>) -> pg_sys::Datum {
                boxret_via_into_datum(fcinfo, ret)
            }
        })*
    };
}

impl_boxret_via_intodatum! {
    String, CString, Json, Inet, Uuid, AnyNumeric, Vec<u8>,
    Date, Interval, Time, TimeWithTimeZone, Timestamp, TimestampWithTimeZone,
    pg_sys::Oid, pg_sys::BOX, pg_sys::Point, char,
    Internal
}

unsafe impl<const P: u32, const S: u32> BoxRet for crate::Numeric<P, S> {
    type CallRet = Self;
    fn into_ret(self) -> Ret<Self>
    where
        Self: Sized,
    {
        Ret::Once(self)
    }

    unsafe fn box_return(fcinfo: pg_sys::FunctionCallInfo, ret: Ret<Self>) -> pg_sys::Datum {
        boxret_via_into_datum(fcinfo, ret)
    }
}

unsafe impl<T> BoxRet for crate::Range<T>
where
    T: IntoDatum + crate::RangeSubType,
{
    type CallRet = Self;
    fn into_ret(self) -> Ret<Self>
    where
        Self: Sized,
    {
        Ret::Once(self)
    }

    unsafe fn box_return(fcinfo: pg_sys::FunctionCallInfo, ret: Ret<Self>) -> pg_sys::Datum {
        boxret_via_into_datum(fcinfo, ret)
    }
}

unsafe impl<T> BoxRet for Vec<T>
where
    T: IntoDatum,
{
    type CallRet = Self;
    fn into_ret(self) -> Ret<Self>
    where
        Self: Sized,
    {
        Ret::Once(self)
    }

    unsafe fn box_return(fcinfo: pg_sys::FunctionCallInfo, ret: Ret<Self>) -> pg_sys::Datum {
        boxret_via_into_datum(fcinfo, ret)
    }
}

unsafe impl<T: Copy> BoxRet for PgVarlena<T> {
    type CallRet = Self;
    fn into_ret(self) -> Ret<Self>
    where
        Self: Sized,
    {
        Ret::Once(self)
    }

    unsafe fn box_return(fcinfo: pg_sys::FunctionCallInfo, ret: Ret<Self>) -> pg_sys::Datum {
        boxret_via_into_datum(fcinfo, ret)
    }
}

unsafe impl<'mcx, A> BoxRet for PgHeapTuple<'mcx, A>
where
    A: crate::WhoAllocated,
{
    type CallRet = Self;
    fn into_ret(self) -> Ret<Self>
    where
        Self: Sized,
    {
        Ret::Once(self)
    }

    unsafe fn box_return(fcinfo: pg_sys::FunctionCallInfo, ret: Ret<Self>) -> pg_sys::Datum {
        boxret_via_into_datum(fcinfo, ret)
    }
}

unsafe impl<T, A> BoxRet for PgBox<T, A>
where
    A: crate::WhoAllocated,
{
    type CallRet = Self;
    fn into_ret(self) -> Ret<Self>
    where
        Self: Sized,
    {
        Ret::Once(self)
    }

    unsafe fn box_return(fcinfo: pg_sys::FunctionCallInfo, ret: Ret<Self>) -> pg_sys::Datum {
        boxret_via_into_datum(fcinfo, ret)
    }
}