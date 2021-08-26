use super::LevelFilter;
use crate::{
    layer::{Context, Layer},
    registry,
};
use std::{any::type_name, cell::Cell, fmt, marker::PhantomData, thread_local};
use tracing_core::{
    span,
    subscriber::{Interest, Subscriber},
    Event, Metadata,
};

/// A filter that determines whether a span or event is enabled.
pub trait Filter<S> {
    fn enabled(&self, meta: &Metadata<'_>, cx: &Context<'_, S>) -> bool;

    fn callsite_enabled(&self, meta: &'static Metadata<'static>) -> Interest {
        let _ = meta;
        Interest::sometimes()
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        None
    }
}

#[derive(Debug, Clone)]
pub struct Filtered<L, F, S> {
    filter: F,
    layer: L,
    id: FilterId,
    _s: PhantomData<fn(S)>,
}

pub struct FilterFn<
    S,
    F = fn(&Metadata<'_>, &Context<'_, S>) -> bool,
    R = fn(&'static Metadata<'static>) -> Interest,
> {
    enabled: F,
    register_callsite: Option<R>,
    max_level_hint: Option<LevelFilter>,
    _s: PhantomData<fn(S)>,
}

#[derive(Copy, Clone, Debug)]
pub struct FilterId(u8);

#[derive(Default, Copy, Clone)]
pub(crate) struct FilterMap {
    bits: u64,
}

thread_local! {
    /// The current state of `enabled` calls to per-layer filters on this
    /// thread.
    ///
    /// When `Filtered::enabled` is called, the filter will set the bit
    /// corresponding to its ID if it will disable the event/span being
    /// filtered. When the event or span is recorded, the per-layer filter will
    /// check its bit to determine if it disabled that event or span, and skip
    /// forwarding the event or span to the inner layer if the bit is set. Once
    /// a span or event has been skipped by a per-layer filter, it unsets its
    /// bit, so that the `FilterMap` has been cleared for the next set of
    /// `enabled` calls.
    ///
    /// This is also read by the `Registry`, for two reasons:
    ///
    /// 1. When filtering a span, the registry must store the `FilterMap`
    ///    generated by `Filtered::enabled` calls for that span as part of the
    ///    span's per-span data. This allows `Filtered` layers to determine
    ///    whether they previously disabled a given span, and avoid showing it
    ///    to the wrapped layer if it was disabled.
    ///
    ///    This is the mechanism that allows `Filtered` layers to also filter
    ///    out the spans they disable from span traversals (such as iterating
    ///    over parents, etc).
    /// 2. If all the bits are set, then every per-layer filter has decided it
    ///    doesn't want to enable that span or event. In that case, the
    ///    `Registry`'s `enabled` method will return `false`, so that we can
    ///    skip recording it entirely.
    pub(crate) static FILTERING: Cell<FilterMap> = Cell::new(FilterMap::default());
    pub(crate) static INTERESTING: Cell<Interest> = Cell::new(Interest::never());
}

// === impl Filter ===

impl<S> Filter<S> for LevelFilter {
    fn enabled(&self, meta: &Metadata<'_>, _: &Context<'_, S>) -> bool {
        meta.level() <= self
    }

    fn callsite_enabled(&self, meta: &'static Metadata<'static>) -> Interest {
        if meta.level() <= self {
            Interest::always()
        } else {
            Interest::never()
        }
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(*self)
    }
}

// === impl Filtered ===

impl<L, F, S> Filtered<L, F, S> {
    pub fn new(layer: L, filter: F) -> Self {
        Self {
            layer,
            filter,
            id: FilterId(255),
            _s: PhantomData,
        }
    }

    fn did_enable(&self, f: impl FnOnce()) {
        FILTERING.with(|filtering| {
            if filtering.get().is_enabled(self.id) {
                f();

                filtering.set(filtering.get().set(self.id, true));
            }
        })
    }
}

impl<S, L, F> Layer<S> for Filtered<L, F, S>
where
    S: Subscriber + for<'span> registry::LookupSpan<'span> + 'static,
    F: Filter<S> + 'static,
    L: Layer<S>,
{
    fn on_register(&mut self, subscriber: &mut S) {
        self.id = subscriber.register_filter();
        self.layer.on_register(subscriber);
    }

    // TODO(eliza): can we figure out a nice way to make the `Filtered` layer
    // not call `is_enabled_for` in hooks that the inner layer doesn't actually
    // have real implementations of? probably not...
    //
    // it would be cool if there was some wild rust reflection way of checking
    // if a trait impl has the default impl of a trait method or not, but that's
    // almsot certainly impossible...right?

    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        // self.filter.callsite_enabled(metadata)
        Interest::sometimes()
    }

    fn enabled(&self, metadata: &Metadata<'_>, cx: Context<'_, S>) -> bool {
        let enabled = self.filter.enabled(metadata, &cx.with_filter(self.id));
        FILTERING.with(|filtering| filtering.set(filtering.get().set(self.id, enabled)));
        true // keep filtering
    }

    fn new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, cx: Context<'_, S>) {
        self.did_enable(|| {
            self.layer.new_span(attrs, id, cx.with_filter(self.id));
        })
    }

    #[doc(hidden)]
    fn max_level_hint(&self) -> Option<LevelFilter> {
        self.filter.max_level_hint()
    }

    fn on_record(&self, span: &span::Id, values: &span::Record<'_>, cx: Context<'_, S>) {
        if let Some(cx) = cx.if_enabled_for(span, self.id) {
            self.layer.on_record(span, values, cx)
        }
    }

    fn on_follows_from(&self, span: &span::Id, follows: &span::Id, cx: Context<'_, S>) {
        // only call `on_follows_from` if both spans are enabled by us
        if cx.is_enabled_for(span, self.id) && cx.is_enabled_for(follows, self.id) {
            self.layer
                .on_follows_from(span, follows, cx.with_filter(self.id))
        }
    }

    fn on_event(&self, event: &Event<'_>, cx: Context<'_, S>) {
        self.did_enable(|| {
            self.layer.on_event(event, cx.with_filter(self.id));
        })
    }

    fn on_enter(&self, id: &span::Id, cx: Context<'_, S>) {
        if let Some(cx) = cx.if_enabled_for(id, self.id) {
            self.layer.on_enter(id, cx)
        }
    }

    fn on_exit(&self, id: &span::Id, cx: Context<'_, S>) {
        if let Some(cx) = cx.if_enabled_for(id, self.id) {
            self.layer.on_exit(id, cx)
        }
    }

    fn on_close(&self, id: span::Id, cx: Context<'_, S>) {
        if let Some(cx) = cx.if_enabled_for(&id, self.id) {
            self.layer.on_close(id, cx)
        }
    }

    // XXX(eliza): the existence of this method still makes me sad...
    fn on_id_change(&self, old: &span::Id, new: &span::Id, cx: Context<'_, S>) {
        if let Some(cx) = cx.if_enabled_for(old, self.id) {
            self.layer.on_id_change(old, new, cx)
        }
    }
}

// === impl FilterFn ===

pub fn filter_fn<S, F>(f: F) -> FilterFn<S, F>
where
    F: Fn(&Metadata<'_>, &Context<'_, S>) -> bool,
{
    FilterFn::new(f)
}

impl<S, F> FilterFn<S, F>
where
    F: Fn(&Metadata<'_>, &Context<'_, S>) -> bool,
{
    pub fn new(enabled: F) -> Self {
        Self {
            enabled,
            register_callsite: None,
            max_level_hint: None,
            _s: PhantomData,
        }
    }
}

impl<S, F, R> FilterFn<S, F, R>
where
    F: Fn(&Metadata<'_>, &Context<'_, S>) -> bool,
{
    pub fn with_max_level_hint(self, max_level_hint: LevelFilter) -> Self {
        Self {
            max_level_hint: Some(max_level_hint),
            ..self
        }
    }

    pub fn with_callsite_filter<R2>(self, callsite_enabled: R2) -> FilterFn<S, F, R2>
    where
        R2: Fn(&'static Metadata<'static>) -> Interest,
    {
        let register_callsite = Some(callsite_enabled);
        let FilterFn {
            enabled,
            max_level_hint,
            _s,
            ..
        } = self;
        FilterFn {
            enabled,
            register_callsite,
            max_level_hint,
            _s,
        }
    }

    fn is_below_max_level(&self, metadata: &Metadata<'_>) -> bool {
        self.max_level_hint
            .as_ref()
            .map(|hint| metadata.level() <= hint)
            .unwrap_or(true)
    }

    fn default_callsite_enabled(&self, metadata: &Metadata<'_>) -> Interest {
        if (self.enabled)(metadata, &Context::none()) {
            Interest::always()
        } else {
            Interest::never()
        }
    }
}

impl<S, F, R> Filter<S> for FilterFn<S, F, R>
where
    F: Fn(&Metadata<'_>, &Context<'_, S>) -> bool,
    R: Fn(&'static Metadata<'static>) -> Interest,
{
    fn enabled(&self, metadata: &Metadata<'_>, cx: &Context<'_, S>) -> bool {
        let enabled = (self.enabled)(metadata, cx);
        debug_assert!(
            !enabled || self.is_below_max_level(metadata),
            "FilterFn<{}> claimed it would only enable {:?} and below, \
            but it enabled metadata with the {:?} level\nmetadata={:#?}",
            type_name::<F>(),
            self.max_level_hint.unwrap(),
            metadata.level(),
            metadata,
        );

        enabled
    }

    fn callsite_enabled(&self, metadata: &'static Metadata<'static>) -> Interest {
        let interest = self
            .register_callsite
            .as_ref()
            .map(|callsite_enabled| callsite_enabled(metadata))
            .unwrap_or_else(|| self.default_callsite_enabled(metadata));
        debug_assert!(
            interest.is_never() || self.is_below_max_level(metadata),
            "FilterFn<{}, {}> claimed it was only interested in {:?} and below, \
            but it enabled metadata with the {:?} level\nmetadata={:#?}",
            type_name::<F>(),
            type_name::<R>(),
            self.max_level_hint.unwrap(),
            metadata.level(),
            metadata,
        );

        interest
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        self.max_level_hint
    }
}

impl<S, F, R> fmt::Debug for FilterFn<S, F, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("FilterFn");
        s.field("enabled", &format_args!("{}", type_name::<F>()));
        if self.register_callsite.is_some() {
            s.field(
                "register_callsite",
                &format_args!("Some({})", type_name::<R>()),
            );
        } else {
            s.field("register_callsite", &format_args!("None"));
        }

        s.field("max_level_hint", &self.max_level_hint)
            .field("subscriber_type", &format_args!("{}", type_name::<S>()))
            .finish()
    }
}

impl<S, F, R> Clone for FilterFn<S, F, R>
where
    F: Clone,
    R: Clone,
{
    fn clone(&self) -> Self {
        Self {
            enabled: self.enabled.clone(),
            register_callsite: self.register_callsite.clone(),
            max_level_hint: self.max_level_hint,
            _s: PhantomData,
        }
    }
}

impl<F, S> From<F> for FilterFn<S, F>
where
    F: Fn(&Metadata<'_>, &Context<'_, S>) -> bool,
{
    fn from(f: F) -> Self {
        Self::new(f)
    }
}

// === impl FilterId ===

impl FilterId {
    pub(crate) fn new(id: u8) -> Self {
        assert!(id < 64, "filter IDs may not be greater than 64");
        Self(id)
    }
}

// === impl FilterMap ===

impl FilterMap {
    pub(crate) fn set(self, FilterId(idx): FilterId, enabled: bool) -> Self {
        debug_assert!(idx < 64 || idx == 255);
        if idx >= 64 {
            return self;
        }

        if enabled {
            Self {
                bits: self.bits & !(1 << idx),
            }
        } else {
            Self {
                bits: self.bits | (1 << idx),
            }
        }
    }

    pub(crate) fn is_enabled(self, FilterId(idx): FilterId) -> bool {
        debug_assert!(idx < 64 || idx == 255);
        if idx >= 64 {
            return false;
        }

        self.bits & (1 << idx) == 0
    }

    pub(crate) fn any_enabled(self) -> bool {
        self.bits != u64::MAX
    }
}

impl fmt::Debug for FilterMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FilterMap")
            .field("bits", &format_args!("{:#b}", self.bits))
            .finish()
    }
}