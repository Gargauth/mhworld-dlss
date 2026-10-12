//! Root signature layout: which descriptor of a table is which register.
//!
//! A root descriptor table reaches the command list as one GPU handle; which
//! descriptor in it is `t4` is decided by the root signature, whose ranges
//! each sit at their own `OffsetInDescriptorsFromTableStart`. The temporal
//! pass's table is `root0[u0..8@+0 t0..31@+9]` (measured): the UAV at 0, the
//! SRVs from 9, so "registers follow the base in order" mislabels every input.
//!
//! The Windows side deserialises the blob into [`Param`]s; this module does
//! the offset arithmetic.

/// `D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND`.
pub const APPEND: u32 = u32::MAX;
/// `NumDescriptors` of an unbounded range.
pub const UNBOUNDED: u32 = u32::MAX;

/// Descriptor range type (`D3D12_DESCRIPTOR_RANGE_TYPE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RangeKind {
    /// `t` registers.
    Srv,
    /// `u` registers.
    Uav,
    /// `b` registers.
    Cbv,
    /// `s` registers.
    Sampler,
}

impl RangeKind {
    /// From the raw `D3D12_DESCRIPTOR_RANGE_TYPE`.
    pub fn from_raw(raw: i32) -> Option<RangeKind> {
        Some(match raw {
            0 => RangeKind::Srv,
            1 => RangeKind::Uav,
            2 => RangeKind::Cbv,
            3 => RangeKind::Sampler,
            _ => return None,
        })
    }

    /// HLSL register prefix.
    pub fn prefix(self) -> char {
        match self {
            RangeKind::Srv => 't',
            RangeKind::Uav => 'u',
            RangeKind::Cbv => 'b',
            RangeKind::Sampler => 's',
        }
    }
}

/// One descriptor range as declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeDesc {
    /// Register class.
    pub kind: RangeKind,
    /// Descriptors in the range, or [`UNBOUNDED`].
    pub count: u32,
    /// First register.
    pub base_register: u32,
    /// Register space.
    pub space: u32,
    /// Offset from the table start, or [`APPEND`].
    pub offset: u32,
}

/// One root parameter as declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Param {
    /// A descriptor table.
    Table(Vec<RangeDesc>),
    /// A root constant buffer view.
    Cbv {
        /// `b` register.
        register: u32,
        /// Space.
        space: u32,
    },
    /// A root shader resource view.
    Srv {
        /// `t` register.
        register: u32,
        /// Space.
        space: u32,
    },
    /// A root unordered access view.
    Uav {
        /// `u` register.
        register: u32,
        /// Space.
        space: u32,
    },
    /// Inline 32-bit constants.
    Constants {
        /// `b` register.
        register: u32,
        /// Space.
        space: u32,
        /// Number of 32-bit values.
        count: u32,
    },
    /// A parameter type this crate does not interpret; it keeps its index.
    Other,
}

/// A range with its offset resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    /// Register class.
    pub kind: RangeKind,
    /// Descriptors in the range, or [`UNBOUNDED`].
    pub count: u32,
    /// First register.
    pub base_register: u32,
    /// Register space.
    pub space: u32,
    /// Offset in descriptors from the table start.
    pub offset: u32,
}

impl Range {
    /// The table offset of `register`, if this range covers it.
    pub fn offset_of(&self, kind: RangeKind, register: u32, space: u32) -> Option<u32> {
        if kind != self.kind || space != self.space || register < self.base_register {
            return None;
        }
        let step = register - self.base_register;
        if self.count != UNBOUNDED && step >= self.count {
            return None;
        }
        self.offset.checked_add(step)
    }
}

/// Where a register lives in a root signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableSlot {
    /// Root parameter index of the table.
    pub root: u32,
    /// Offset in descriptors from the table start.
    pub offset: u32,
}

/// A root signature's parameters with table offsets resolved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layout {
    params: Vec<Resolved>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Resolved {
    Table(Vec<Range>),
    Other(Param),
}

impl Layout {
    /// Resolve `APPEND` offsets: each appended range starts where the
    /// previous one ended (an unbounded range has no end, so nothing can be
    /// appended after it; such a range gets offset `APPEND` and matches
    /// nothing).
    pub fn new(params: &[Param]) -> Layout {
        let params = params
            .iter()
            .map(|param| match param {
                Param::Table(ranges) => {
                    let mut running = Some(0u32);
                    let resolved = ranges
                        .iter()
                        .map(|range| {
                            let offset = match range.offset {
                                APPEND => running,
                                explicit => Some(explicit),
                            };
                            running = match (offset, range.count) {
                                (_, UNBOUNDED) | (None, _) => None,
                                (Some(start), count) => start.checked_add(count),
                            };
                            Range {
                                kind: range.kind,
                                count: range.count,
                                base_register: range.base_register,
                                space: range.space,
                                offset: offset.unwrap_or(APPEND),
                            }
                        })
                        .collect();
                    Resolved::Table(resolved)
                }
                other => Resolved::Other(other.clone()),
            })
            .collect();
        Layout { params }
    }

    /// Number of root parameters.
    pub fn len(&self) -> usize {
        self.params.len()
    }

    /// Whether there are no root parameters.
    pub fn is_empty(&self) -> bool {
        self.params.is_empty()
    }

    /// The table slot holding `kind` register `register` in `space`.
    pub fn table_slot(&self, kind: RangeKind, register: u32, space: u32) -> Option<TableSlot> {
        self.tables().find_map(|(root, ranges)| {
            ranges
                .iter()
                .filter(|range| range.offset != APPEND)
                .find_map(|range| range.offset_of(kind, register, space))
                .map(|offset| TableSlot { root, offset })
        })
    }

    /// The root parameter that is a root CBV for `b{register}` in `space`.
    pub fn root_cbv(&self, register: u32, space: u32) -> Option<u32> {
        self.params
            .iter()
            .enumerate()
            .find_map(|(root, param)| match param {
                Resolved::Other(Param::Cbv {
                    register: r,
                    space: s,
                }) if *r == register && *s == space => u32::try_from(root).ok(),
                _ => None,
            })
    }

    /// Every table, as `(root parameter, ranges)`.
    pub fn tables(&self) -> impl Iterator<Item = (u32, &[Range])> {
        self.params
            .iter()
            .enumerate()
            .filter_map(|(root, param)| match param {
                Resolved::Table(ranges) => Some((u32::try_from(root).ok()?, ranges.as_slice())),
                Resolved::Other(_) => None,
            })
    }

    /// `root0[u0..7@+0 t0..30@+9] root2=b0 root3=b1`, for the log.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        for (root, param) in self.params.iter().enumerate() {
            match param {
                Resolved::Table(ranges) => {
                    let listed: Vec<String> = ranges
                        .iter()
                        .map(|r| {
                            let last = if r.count == UNBOUNDED {
                                "*".to_string()
                            } else {
                                (r.base_register + r.count.saturating_sub(1)).to_string()
                            };
                            format!(
                                "{}{}..{}@+{}",
                                r.kind.prefix(),
                                r.base_register,
                                last,
                                r.offset
                            )
                        })
                        .collect();
                    parts.push(format!("root{root}[{}]", listed.join(" ")));
                }
                Resolved::Other(Param::Cbv { register, .. }) => {
                    parts.push(format!("root{root}=b{register}"))
                }
                Resolved::Other(Param::Srv { register, .. }) => {
                    parts.push(format!("root{root}=t{register}"))
                }
                Resolved::Other(Param::Uav { register, .. }) => {
                    parts.push(format!("root{root}=u{register}"))
                }
                Resolved::Other(Param::Constants {
                    register, count, ..
                }) => parts.push(format!("root{root}=b{register}x{count}")),
                Resolved::Other(Param::Other) => parts.push(format!("root{root}=?")),
                Resolved::Other(Param::Table(_)) => {}
            }
        }
        parts.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(kind: RangeKind, count: u32, base: u32, offset: u32) -> RangeDesc {
        RangeDesc {
            kind,
            count,
            base_register: base,
            space: 0,
            offset,
        }
    }

    /// The temporal pass's measured layout: a table with the UAVs at 0 and
    /// the SRVs at 9, then root CBVs for the camera and the screen.
    fn temporal() -> Layout {
        Layout::new(&[
            Param::Table(vec![
                range(RangeKind::Uav, 8, 0, 0),
                range(RangeKind::Srv, 31, 0, 9),
            ]),
            Param::Constants {
                register: 5,
                space: 0,
                count: 4,
            },
            Param::Cbv {
                register: 0,
                space: 0,
            },
            Param::Cbv {
                register: 1,
                space: 0,
            },
        ])
    }

    #[test]
    fn explicit_offsets_place_registers() {
        let layout = temporal();
        let at = |kind, register| layout.table_slot(kind, register, 0);
        assert_eq!(
            at(RangeKind::Uav, 0),
            Some(TableSlot { root: 0, offset: 0 })
        );
        assert_eq!(
            at(RangeKind::Srv, 0),
            Some(TableSlot { root: 0, offset: 9 })
        );
        assert_eq!(
            at(RangeKind::Srv, 4),
            Some(TableSlot {
                root: 0,
                offset: 13
            })
        );
        assert_eq!(at(RangeKind::Srv, 31), None);
        assert_eq!(at(RangeKind::Uav, 8), None);
        assert_eq!(at(RangeKind::Cbv, 0), None);
        assert_eq!(layout.table_slot(RangeKind::Srv, 0, 1), None);
        assert_eq!(layout.root_cbv(0, 0), Some(2));
        assert_eq!(layout.root_cbv(1, 0), Some(3));
        assert_eq!(layout.root_cbv(5, 0), None, "root constants are not a CBV");
        assert_eq!(
            layout.summary(),
            "root0[u0..7@+0 t0..30@+9] root1=b5x4 root2=b0 root3=b1"
        );
    }

    #[test]
    fn appended_ranges_follow_the_previous_one() {
        let layout = Layout::new(&[
            Param::Cbv {
                register: 0,
                space: 0,
            },
            Param::Table(vec![
                range(RangeKind::Srv, 2, 0, APPEND),
                range(RangeKind::Uav, 3, 0, APPEND),
                range(RangeKind::Srv, 1, 7, 20),
                range(RangeKind::Cbv, 1, 1, APPEND),
            ]),
        ]);
        let at = |kind, register| layout.table_slot(kind, register, 0);
        assert_eq!(
            at(RangeKind::Srv, 1),
            Some(TableSlot { root: 1, offset: 1 })
        );
        assert_eq!(
            at(RangeKind::Uav, 0),
            Some(TableSlot { root: 1, offset: 2 })
        );
        assert_eq!(
            at(RangeKind::Uav, 2),
            Some(TableSlot { root: 1, offset: 4 })
        );
        assert_eq!(
            at(RangeKind::Srv, 7),
            Some(TableSlot {
                root: 1,
                offset: 20
            })
        );
        assert_eq!(
            at(RangeKind::Cbv, 1),
            Some(TableSlot {
                root: 1,
                offset: 21
            })
        );
    }

    #[test]
    fn nothing_is_appended_after_an_unbounded_range() {
        let layout = Layout::new(&[Param::Table(vec![
            range(RangeKind::Srv, UNBOUNDED, 0, 0),
            range(RangeKind::Uav, 1, 0, APPEND),
        ])]);
        assert_eq!(
            layout.table_slot(RangeKind::Srv, 1000, 0),
            Some(TableSlot {
                root: 0,
                offset: 1000
            })
        );
        assert_eq!(layout.table_slot(RangeKind::Uav, 0, 0), None);
    }
}
