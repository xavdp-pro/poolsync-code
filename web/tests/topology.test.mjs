import test from 'node:test'
import assert from 'node:assert/strict'
import { inferNeighbors, scaleLayout, nodeRect, connectionLines, snapPosition } from '../src/topologyLayout.js'
const n = (x, y, width, height, kvm_enabled = true) => ({ x, y, width, height, kvm_enabled, neighbors: {} })
test('negative positions and mixed resolutions remain visible at the same scale', () => {
  const nodes = { laptop: n(-1366, -200, 1366, 768), desktop: n(0, 0, 2560, 1440), clip: n(0, 100000, 1920, 1080, false) }
  const topo = inferNeighbors({ nodes }); const layout = scaleLayout(nodes)
  assert.equal(topo.nodes.laptop.neighbors.right, 'desktop')
  assert.equal(nodeRect(nodes.laptop, layout.scale, layout).left, 24)
  assert.ok(layout.width <= 768)
  assert.ok(connectionLines(topo.nodes, layout.scale, layout).every(l => l.x1 >= 24 && l.x2 >= 24))
  assert.deepEqual(snapPosition(-31, -29), [-40, -20])
})
test('ambiguous edges choose greatest overlap independently of insertion order', () => {
  const nodes = { a: n(0, 0, 1920, 1080), b: n(1920, 900, 800, 600), c: n(1920, 0, 2560, 1440) }
  const forward = inferNeighbors({ nodes }); const backward = inferNeighbors({ nodes: Object.fromEntries(Object.entries(nodes).reverse()) })
  assert.equal(forward.nodes.a.neighbors.right, 'c')
  assert.equal(forward.nodes.c.neighbors.left, 'a')
  for (const id of Object.keys(nodes)) assert.deepEqual(forward.nodes[id].neighbors, backward.nodes[id].neighbors)
})
test('disabling a temporarily absent device leaves other routes and positions intact', () => {
  const nodes = { a: n(0, 0, 800, 600), b: n(800, 0, 800, 600), absent: n(1600, 0, 800, 600, false) }
  const topo = inferNeighbors({ nodes }); assert.equal(topo.nodes.a.neighbors.right, 'b')
  assert.equal(topo.nodes.b.neighbors.right, undefined); assert.equal(topo.nodes.absent.x, 1600)
})
