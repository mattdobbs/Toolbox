import Foundation

enum SplitAxis: Equatable {
    /// Panes side by side.
    case horizontal
    /// Panes stacked.
    case vertical
}

/// The layout of one workspace: a tree of splits whose leaves are panes.
indirect enum PaneTree: Equatable {
    case leaf(UUID)
    case split(SplitAxis, [PaneTree])

    var leaves: [UUID] {
        switch self {
        case .leaf(let id): return [id]
        case .split(_, let children): return children.flatMap(\.leaves)
        }
    }

    /// Returns a tree where `newLeaf` sits next to `target` along `axis`.
    /// When `target` already lives in a split of the same axis, the new leaf
    /// becomes a sibling instead of nesting another split.
    func splitting(_ target: UUID, axis: SplitAxis, newLeaf: UUID) -> PaneTree {
        switch self {
        case .leaf(let id):
            return id == target ? .split(axis, [.leaf(id), .leaf(newLeaf)]) : self
        case .split(let existingAxis, let children):
            var out: [PaneTree] = []
            for child in children {
                if case .leaf(let id) = child, id == target, existingAxis == axis {
                    out.append(child)
                    out.append(.leaf(newLeaf))
                } else {
                    out.append(child.splitting(target, axis: axis, newLeaf: newLeaf))
                }
            }
            return .split(existingAxis, out)
        }
    }

    /// Returns the tree without `target`, collapsing splits left with a single
    /// child. `nil` means the tree became empty.
    func removing(_ target: UUID) -> PaneTree? {
        switch self {
        case .leaf(let id):
            return id == target ? nil : self
        case .split(let axis, let children):
            let kept = children.compactMap { $0.removing(target) }
            if kept.isEmpty { return nil }
            if kept.count == 1 { return kept[0] }
            var flat: [PaneTree] = []
            for node in kept {
                if case .split(let childAxis, let grandchildren) = node, childAxis == axis {
                    flat.append(contentsOf: grandchildren)
                } else {
                    flat.append(node)
                }
            }
            return .split(axis, flat)
        }
    }
}
