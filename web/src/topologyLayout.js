/** Géométrie mosaïque style Barrier (miroir de poolsync-core/src/topology.rs). */

export const EDGE_TOLERANCE_PX = 48
export const SNAP_GRID_PX = 20
export const MIN_EDGE_OVERLAP_PX = 80
export const CANVAS_PAD = 24

export function snapPosition(x, y, grid = SNAP_GRID_PX) {
  const g = Math.max(1, grid)
  return [
    Math.sign(x) * Math.round(Math.abs(x) / g) * g,
    Math.sign(y) * Math.round(Math.abs(y) / g) * g,
  ]
}

function overlapLen(a0, a1, b0, b1) {
  return Math.max(0, Math.min(a1, b1) - Math.max(a0, b0))
}

function setNeighbor(nodes, id, dir, other) {
  if (!nodes[id].neighbors) nodes[id].neighbors = {}
  nodes[id].neighbors[dir] = other
}

/** Recalcule les voisins bidirectionnels à partir des positions. */
export function inferNeighbors(topology, tolerancePx = EDGE_TOLERANCE_PX) {
  const tol = Math.max(1, tolerancePx)
  const ids = Object.keys(topology?.nodes || {}).filter(
    (id) => topology.nodes[id]?.kvm_enabled !== false,
  )
  ids.sort()
  const candidates = []
  const nodes = {}
  for (const id of Object.keys(topology?.nodes || {})) {
    nodes[id] = { ...topology.nodes[id], neighbors: {} }
  }

  for (let i = 0; i < ids.length; i += 1) {
    for (let j = i + 1; j < ids.length; j += 1) {
      const aId = ids[i]
      const bId = ids[j]
      const a = nodes[aId]
      const b = nodes[bId]
      const aRight = a.x + a.width
      const bRight = b.x + b.width
      const aBottom = a.y + a.height
      const bBottom = b.y + b.height
      const vOverlap = overlapLen(a.y, aBottom, b.y, bBottom)
      const hOverlap = overlapLen(a.x, aRight, b.x, bRight)

      if (Math.abs(b.x - aRight) <= tol && vOverlap >= MIN_EDGE_OVERLAP_PX) {
        candidates.push({ a: aId, b: bId, dir: 'right', reverse: 'left', gap: Math.abs(b.x - aRight), overlap: vOverlap })
      }
      if (Math.abs(a.x - bRight) <= tol && vOverlap >= MIN_EDGE_OVERLAP_PX) {
        candidates.push({ a: aId, b: bId, dir: 'left', reverse: 'right', gap: Math.abs(a.x - bRight), overlap: vOverlap })
      }
      if (Math.abs(b.y - aBottom) <= tol && hOverlap >= MIN_EDGE_OVERLAP_PX) {
        candidates.push({ a: aId, b: bId, dir: 'down', reverse: 'up', gap: Math.abs(b.y - aBottom), overlap: hOverlap })
      }
      if (Math.abs(a.y - bBottom) <= tol && hOverlap >= MIN_EDGE_OVERLAP_PX) {
        candidates.push({ a: aId, b: bId, dir: 'up', reverse: 'down', gap: Math.abs(a.y - bBottom), overlap: hOverlap })
      }
    }
  }
  candidates.sort((a, b) => a.gap - b.gap || b.overlap - a.overlap ||
    (a.a < b.a ? -1 : a.a > b.a ? 1 : 0) || (a.b < b.b ? -1 : a.b > b.b ? 1 : 0) ||
    (a.dir < b.dir ? -1 : a.dir > b.dir ? 1 : 0))
  for (const c of candidates) {
    if (!nodes[c.a].neighbors[c.dir] && !nodes[c.b].neighbors[c.reverse]) {
      setNeighbor(nodes, c.a, c.dir, c.b)
      setNeighbor(nodes, c.b, c.reverse, c.a)
    }
  }
  return { nodes }
}

/**
 * Une machine sans KVM est garée très bas (y = 100000) par le hub, pour la
 * sortir de la mosaïque sans perdre sa position si le KVM est réactivé.
 * Elle ne doit pas entrer dans le calcul d'échelle : sinon les écrans réels
 * se réduisent à quelques pixels dans un canevas presque vide.
 */
export const PARKED_Y = 50000

export function isParked(n) {
  return !n || n.y >= PARKED_Y
}

/**
 * Aligne un écran sur les bords des autres, comme dans un éditeur graphique.
 *
 * L'aimantation sur grille seule ne suffit pas : deux écrans de hauteurs
 * différentes ne se touchent jamais franchement, et les voisins KVM ne se
 * déduisent pas. On propose donc d'abord les bords des autres écrans, et on
 * retombe sur la grille si aucun n'est assez proche.
 */
export function snapToNeighbors(nodes, id, x, y, tolerance = 24) {
  const self = nodes[id]
  if (!self) return snapPosition(x, y)
  const others = Object.entries(nodes).filter(([oid, n]) => oid !== id && !isParked(n))
  let bestX = null
  let bestY = null
  const consider = (candidate, current, best) => {
    const d = Math.abs(candidate - current)
    if (d <= tolerance && (best === null || d < Math.abs(best - current))) return candidate
    return best
  }
  for (const [, o] of others) {
    // bord à bord horizontalement, ou alignement des bords gauche/droit
    bestX = consider(o.x + o.width, x, bestX)
    bestX = consider(o.x - self.width, x, bestX)
    bestX = consider(o.x, x, bestX)
    bestX = consider(o.x + o.width - self.width, x, bestX)
    // idem verticalement
    bestY = consider(o.y + o.height, y, bestY)
    bestY = consider(o.y - self.height, y, bestY)
    bestY = consider(o.y, y, bestY)
    bestY = consider(o.y + o.height - self.height, y, bestY)
  }
  const [gx, gy] = snapPosition(x, y)
  return [bestX ?? gx, bestY ?? gy]
}

export function scaleLayout(nodes, maxW = 720, maxH = 420) {
  const all = Object.entries(nodes || {})
  const entries = all.filter(([, n]) => !isParked(n))
  if (!entries.length) {
    return { scale: 0.2, width: 400, height: 200, maxX: 0, maxY: 0 }
  }
  const minX = Math.min(...entries.map(([, n]) => n.x))
  const minY = Math.min(...entries.map(([, n]) => n.y))
  let maxX = minX
  let maxY = minY
  for (const [, n] of entries) {
    maxX = Math.max(maxX, n.x + n.width)
    maxY = Math.max(maxY, n.y + n.height)
  }
  const scale = Math.min(maxW / Math.max(maxX - minX, 1), maxH / Math.max(maxY - minY, 1), 0.4)
  return {
    scale,
    width: (maxX - minX) * scale + CANVAS_PAD * 2,
    height: (maxY - minY) * scale + CANVAS_PAD * 2,
    maxX,
    maxY,
    minX,
    minY,
  }
}

/** Segments SVG entre écrans voisins (centre des bords partagés). */
export function connectionLines(nodes, scale, origin = {}) {
  nodes = Object.fromEntries(Object.entries(nodes || {}).map(([id, n]) => [id, { ...n, x: n.x - (origin.minX || 0), y: n.y - (origin.minY || 0) }]))
  const lines = []
  const seen = new Set()
  const pad = CANVAS_PAD

  const cx = (n) => pad + (n.x + n.width / 2) * scale
  const cy = (n) => pad + (n.y + n.height / 2) * scale
  const edge = (n, dir) => {
    switch (dir) {
      case 'left':
        return [pad + n.x * scale, cy(n)]
      case 'right':
        return [pad + (n.x + n.width) * scale, cy(n)]
      case 'up':
        return [cx(n), pad + n.y * scale]
      case 'down':
        return [cx(n), pad + (n.y + n.height) * scale]
      default:
        return [cx(n), cy(n)]
    }
  }

  for (const [id, n] of Object.entries(nodes || {})) {
    for (const dir of ['left', 'right', 'up', 'down']) {
      const other = n.neighbors?.[dir]
      if (!other || !nodes[other]) continue
      const key = [id, other].sort().join('|')
      if (seen.has(key)) continue
      seen.add(key)
      const [x1, y1] = edge(n, dir)
      const opp = { left: 'right', right: 'left', up: 'down', down: 'up' }[dir]
      const [x2, y2] = edge(nodes[other], opp)
      lines.push({ x1, y1, x2, y2, a: id, b: other })
    }
  }
  return lines
}

export function nodeRect(n, scale, origin = {}) {
  return {
    left: CANVAS_PAD + (n.x - (origin.minX || 0)) * scale,
    top: CANVAS_PAD + (n.y - (origin.minY || 0)) * scale,
    width: n.width * scale,
    height: n.height * scale,
  }
}
