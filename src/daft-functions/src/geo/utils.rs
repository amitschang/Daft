use std::sync::Arc;

use arrow2::types::NativeType;
use common_error::{DaftError, DaftResult};
use daft_core::{
    array::ListArray,
    datatypes::logical::GeometryArray,
    prelude::{BinaryArray, DataType, Field},
    series::{IntoSeries, Series},
};
use geo::{Area, BooleanOps, Contains, ConvexHull, EuclideanDistance, Geometry, Intersects};
use geozero::{wkb, wkt, CoordDimensions, ToGeo, ToWkb, ToWkt};
#[cfg(feature = "python")]
use pyo3::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "python", pyclass(module = "daft.daft"))]
pub enum GeoOperation {
    Area,
    ConvexHull,
    Distance,
    Intersects,
    Intersection,
    Contains,
}

pub struct GeometryArrayIter<'a> {
    cursor: usize,
    physical: &'a GeometryArray,
}

impl GeometryArrayIter<'_> {
    pub fn new(physical: &GeometryArray) -> GeometryArrayIter {
        GeometryArrayIter {
            cursor: 0,
            physical,
        }
    }
}

impl<'a> Iterator for GeometryArrayIter<'a> {
    type Item = Option<Geometry>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor >= self.physical.len() {
            None
        } else {
            let x = self.physical.physical.get(self.cursor);
            self.cursor += 1;
            match x {
                Some(x) => {
                    let bytes = x.u8().unwrap().as_slice();
                    Some(Some(wkb::Wkb(bytes).to_geo().unwrap()))
                }
                None => Some(None),
            }
        }
    }
}

struct GH {
    geo_vec: Vec<u8>,
    offsets: Vec<i64>,
    validity: arrow2::bitmap::MutableBitmap,
}

impl GH {
    fn new(capacity: usize) -> Self {
        let mut x = Self {
            geo_vec: Vec::with_capacity(capacity),
            offsets: Vec::with_capacity(capacity + 1),
            validity: arrow2::bitmap::MutableBitmap::with_capacity(capacity),
        };
        x.offsets.push(0i64);
        x
    }

    fn push(&mut self, geo: Geometry) {
        let geo_bytes = geo.to_wkb(CoordDimensions::xy()).unwrap();
        self.geo_vec.extend(geo_bytes.iter());
        self.offsets
            .push(self.offsets.last().unwrap() + geo_bytes.len() as i64);
        self.validity.push(true);
    }

    fn null(&mut self) {
        self.offsets.push(*self.offsets.last().unwrap());
        self.validity.push(false);
    }

    fn into_series(self, name: &str) -> DaftResult<Series> {
        gh_to(name, self)
    }
}

fn gh_to(name: &str, g: GH) -> DaftResult<Series> {
    let data_array = ListArray::new(
        Field::new("data", DataType::List(Box::new(DataType::UInt8))),
        Series::try_from((
            "data",
            Box::new(arrow2::array::PrimitiveArray::from_vec(g.geo_vec))
                as Box<dyn arrow2::array::Array>,
        ))?,
        arrow2::offset::OffsetsBuffer::try_from(g.offsets)?,
        g.validity.into(),
    );
    Ok(GeometryArray::new(Field::new(name, DataType::Geometry), data_array).into_series())
}

pub fn decode_series(s: &Series, raise_error_on_failure: bool) -> DaftResult<Series> {
    match s.data_type() {
        DataType::Binary => {
            let binary = s.binary()?;
            let arrow_array = binary
                .data()
                .as_any()
                .downcast_ref::<arrow2::array::BinaryArray<i64>>()
                .unwrap();
            let mut gh = GH::new(arrow_array.len());
            for bytes in arrow_array.iter() {
                match bytes {
                    Some(bytes) => match wkb::Wkb(bytes).to_geo() {
                        Ok(geo) => gh.push(geo),
                        Err(_) => {
                            if raise_error_on_failure {
                                return Err(DaftError::ValueError(
                                    "Could not decode WKB".to_string(),
                                ));
                            }
                            gh.null()
                        }
                    },
                    None => gh.null(),
                }
            }
            gh.into_series(binary.name())
        }
        DataType::Utf8 => {
            let strings = s.utf8()?;
            let mut gh = GH::new(strings.len());
            let s = strings
                .data()
                .as_any()
                .downcast_ref::<arrow2::array::Utf8Array<i64>>()
                .unwrap();
            for x in s.iter() {
                match x {
                    Some(x) => match wkt::Wkt(x).to_geo() {
                        Ok(geo) => gh.push(geo),
                        Err(_) => {
                            if raise_error_on_failure {
                                return Err(DaftError::ValueError(format!(
                                    "Could not decode WKT text {}",
                                    x
                                )));
                            }
                            gh.null();
                        }
                    },
                    None => gh.null(),
                }
            }
            gh.into_series(strings.name())
        }
        other => Err(DaftError::TypeError(format!(
            "GeoDecode can only decode Binary or Utf8 arrays, got {}",
            other
        ))),
    }
}

pub fn to_wkt(s: &Series) -> DaftResult<Series> {
    let geo = s.geometry()?;
    let mut wkt_vec: Vec<Option<String>> = Vec::with_capacity(geo.len());
    for g in GeometryArrayIter::new(geo) {
        match g {
            Some(g) => {
                let wkt = g.to_wkt().unwrap();
                wkt_vec.push(Some(wkt));
            }
            None => wkt_vec.push(None),
        }
    }
    let utf8_array = arrow2::array::Utf8Array::<i64>::from(wkt_vec);
    Series::from_arrow(
        Arc::new(Field::new(geo.name(), DataType::Utf8)),
        Box::new(utf8_array),
    )
}

pub fn to_wkb(s: &Series) -> DaftResult<Series> {
    let geo = s.geometry()?;
    let mut wkb_vec: Vec<Option<Vec<u8>>> = Vec::with_capacity(geo.len());
    for g in GeometryArrayIter::new(geo) {
        match g {
            Some(g) => {
                let wkb = g.to_wkb(CoordDimensions::xy()).unwrap();
                wkb_vec.push(Some(wkb));
            }
            None => wkb_vec.push(None),
        }
    }
    let bin_array = arrow2::array::BinaryArray::<i64>::from(wkb_vec);
    Ok(BinaryArray::new(
        Arc::new(Field::new(geo.name(), DataType::Binary)),
        Box::new(bin_array),
    )
    .unwrap()
    .into_series())
}

pub fn encode_series(s: &Series, text: bool) -> DaftResult<Series> {
    match text {
        true => to_wkt(s),
        false => to_wkb(s),
    }
}

pub fn geo_unary_dispatch(s: &Series, op: GeoOperation) -> DaftResult<Series> {
    match op {
        GeoOperation::Area => geo_unary_to_scalar::<f64, _>(s, |g| g.unsigned_area()),
        GeoOperation::ConvexHull => geo_unary_to_geo(s, |g| g.convex_hull().into()),
        _ => Err(DaftError::ValueError(format!("unsupported op {:?}", op))),
    }
}

pub fn geo_binary_dispatch(lhs: &Series, rhs: &Series, op: GeoOperation) -> DaftResult<Series> {
    match op {
        GeoOperation::Distance => {
            geo_binary_to_scalar::<f64, _>(lhs, rhs, |l, r| l.euclidean_distance(&r))
        }
        GeoOperation::Intersects => geo_binary_to_bool(lhs, rhs, |l, r| l.intersects(&r)),
        GeoOperation::Contains => geo_binary_to_bool(lhs, rhs, |l, r| l.contains(&r)),
        GeoOperation::Intersection => geo_binary_to_geo(lhs, rhs, |l, r| match (l, r) {
            (Geometry::Polygon(l), Geometry::Polygon(r)) => Some(l.intersection(&r).into()),
            (Geometry::MultiPolygon(l), Geometry::MultiPolygon(r)) => {
                Some(l.intersection(&r).into())
            }
            _ => None,
        }),
        _ => Err(DaftError::ValueError(format!("unsupported op {:?}", op))),
    }
}

pub fn geo_unary_to_scalar<T: NativeType, F>(s: &Series, op_fn: F) -> DaftResult<Series>
where
    F: Fn(Geometry) -> T,
{
    let geo_array = s.geometry()?;
    let scalar_iter = GeometryArrayIter::new(geo_array).map(|geo| geo.map(&op_fn));
    let arrow_array = arrow2::array::PrimitiveArray::<T>::from_iter(scalar_iter);
    Series::from_arrow(
        Arc::new(Field::new(
            geo_array.name(),
            DataType::from(arrow_array.data_type()),
        )),
        Box::new(arrow_array),
    )
}

pub fn geo_unary_to_geo<F>(s: &Series, op_fn: F) -> DaftResult<Series>
where
    F: Fn(Geometry) -> Geometry,
{
    let geo_array = s.geometry()?;
    let mut gh = GH::new(geo_array.len());
    for geo in GeometryArrayIter::new(geo_array) {
        match geo {
            Some(g) => gh.push(op_fn(g)),
            _ => gh.null(),
        }
    }
    gh.into_series(geo_array.name())
}

pub fn geo_binary_to_scalar<T: NativeType, F>(
    lhs: &Series,
    rhs: &Series,
    op_fn: F,
) -> DaftResult<Series>
where
    F: Fn(Geometry, Geometry) -> T,
{
    let lhs_array = lhs.geometry()?;
    let rhs_array = rhs.geometry()?;
    let scalar_iter = GeometryArrayIter::new(lhs_array)
        .zip(GeometryArrayIter::new(rhs_array))
        .map(|(lhg, rhg)| match (lhg, rhg) {
            (Some(l), Some(r)) => Some(op_fn(l, r)),
            _ => None,
        });
    let arrow_array = arrow2::array::PrimitiveArray::<T>::from_iter(scalar_iter);
    Series::from_arrow(
        Arc::new(Field::new(
            lhs_array.name(),
            DataType::from(arrow_array.data_type()),
        )),
        Box::new(arrow_array),
    )
}

pub fn geo_binary_to_bool<F>(lhs: &Series, rhs: &Series, op_fn: F) -> DaftResult<Series>
where
    F: Fn(Geometry, Geometry) -> bool,
{
    let lhs_array = lhs.geometry()?;
    let rhs_array = rhs.geometry()?;
    let scalar_iter = GeometryArrayIter::new(lhs_array)
        .zip(GeometryArrayIter::new(rhs_array))
        .map(|(lhg, rhg)| match (lhg, rhg) {
            (Some(l), Some(r)) => Some(op_fn(l, r)),
            _ => None,
        });
    let arrow_array = arrow2::array::BooleanArray::from_iter(scalar_iter);
    Series::from_arrow(
        Arc::new(Field::new(lhs_array.name(), DataType::Boolean)),
        Box::new(arrow_array),
    )
}

pub fn geo_binary_to_geo<F>(lhs: &Series, rhs: &Series, op_fn: F) -> DaftResult<Series>
where
    F: Fn(Geometry, Geometry) -> Option<Geometry>,
{
    let lhs_array = lhs.geometry()?;
    let rhs_array = rhs.geometry()?;
    let mut gh = GH::new(lhs_array.len());
    for (lhg, rhg) in GeometryArrayIter::new(lhs_array).zip(GeometryArrayIter::new(rhs_array)) {
        match (lhg, rhg) {
            (Some(l), Some(r)) => match op_fn(l, r) {
                Some(g) => gh.push(g),
                None => gh.null(),
            },
            _ => gh.null(),
        }
    }
    gh.into_series(lhs_array.name())
}
