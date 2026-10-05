use {
    crate::{
        Allocator,
        SmallVec
    },
    alloc::alloc::handle_alloc_error,
    core::{
        alloc::Layout,
        error::Error,
        fmt::{
            Debug,
            Display,
            Formatter,
            Result as Format
        }
    }
};

#[derive(Debug)]
pub enum SmallVecError {
    CapacityOverflow,
    AllocationError(Layout)
}

impl Display for SmallVecError {
    fn fmt(&self, f: &mut Formatter<'_>) -> Format {
        write!(f, "Allocation error: {self:?}")
    }
}

impl Error for SmallVecError {}

impl SmallVecError {
    #[cold]
    #[inline(never)]
    pub fn handle<Type>(self) -> Type {
        match self {
            SmallVecError::CapacityOverflow => panic!("capacity overflow"),
            SmallVecError::AllocationError(layout) => handle_alloc_error(layout)
        }
    }
}

/// The error type returned by [`SmallVec::map_in_place`].
///
/// It carries the partially converted vector, the index of the element that
/// failed, and the error returned by the mapper.
///
/// [`SmallVec::map_in_place`]: crate::SmallVec::map_in_place
pub struct MapInPlaceError<T, const N: usize, A: Allocator, E> {
    /// The partially converted vector.
    ///
    /// Elements `0..index` are the new values produced by the mapper.
    /// Element `index` is the recovered value returned by the mapper (it is
    /// whatever the mapper provides, not necessarily the original value).
    /// Elements `index + 1..` are the original, untouched values.
    ///
    /// The vector's original buffer is reused; no reallocation takes place.
    pub vec: SmallVec<T, N, A>,
    /// The index of the element for which the mapper returned an error.
    pub index: usize,
    /// The error returned by the mapper.
    pub error: E
}

impl<T: Debug, const N: usize, A: Allocator, E: Debug> Debug for MapInPlaceError<T, N, A, E> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Format {
        f.debug_struct("MapInPlaceError")
            .field("vec", &self.vec)
            .field("index", &self.index)
            .field("error", &self.error)
            .finish()
    }
}

impl<T, const N: usize, A: Allocator, E: Display> Display for MapInPlaceError<T, N, A, E> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Format {
        write!(f, "map_in_place failed at index {}", self.index)
    }
}

impl<T: Debug, const N: usize, A: Allocator, E: Error> Error for MapInPlaceError<T, N, A, E> {}
