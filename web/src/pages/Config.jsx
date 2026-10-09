import {
  LayoutGrid,
  Link2,
  MonitorSmartphone,
  MousePointer2,
  Redo2,
  Save,
  Settings2,
  Undo2,
  Zap,
} from 'lucide-react'
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import PageHeader from '../components/PageHeader'
import {
  fetchStatus,
  fetchTopology,
  nodeLabel,
  saveTopology,
  showEdges,
} from '../api'
import NodeBadges, { LastClip } from '../components/NodeBadges'
import MonitorMap from '../components/MonitorMap'
import {
  CANVAS_PAD,
  connectionLines,
  inferNeighbors,
  isParked,
  nodeRect,
  scaleLayout,
  snapToNeighbors,
  SNAP_GRID_PX,
} from '../topologyLayout'

const DIRS = ['left', 'right', 'up', 'down']
const DIR_LABEL = { left: '←', right: '→', up: '↑', down: '↓' }

export default function Config({ token, onTokenChange }) {
  const [topology, setTopology] = useState(null)
  const [error, setError] = useState(null)
  const [saved, setSaved] = useState(false)
  const [dragId, setDragId] = useState(null)
  const [dirty, setDirty] = useState(false)
  const [statusError, setStatusError] = useState(null)
  // État vivant des nœuds (en ligne, synchro, pause, maître, dernière copie).
  // Rafraîchi en continu : la mosaïque doit refléter le pool tel qu'il est,
  // pas seulement la géométrie enregistrée.
  const [statusByNode, setStatusByNode] = useState({})
  // Écran sélectionné (déplaçable au clavier) et machine devant laquelle on est.
  const [selected, setSelected] = useState(null)
  const [myNode, setMyNode] = useState(() => localStorage.getItem('poolsync_my_node') || '')
  const dragStart = useRef({})
  const editing = useRef(false)
  editing.current = dirty || !!dragId

  const load = useCallback(async () => {
    try {
      const json = await fetchTopology(token)
      setTopology(json)
      setSelected((id) => json.nodes[id] ? id : Object.keys(json.nodes).sort()[0])
      setPast([]); setFuture([]); setDirty(false)
      setError(null)
    } catch (err) {
      setError(err.message || 'Erreur chargement')
    }
  }, [token])

  useEffect(() => {
    load()
  }, [load])

  useEffect(() => {
    let alive = true
    const refresh = async () => {
      if (document.hidden) return
      try {
        const json = await fetchStatus(token)
        if (!alive) return
        const byNode = {}
        for (const n of json.nodes || []) byNode[n.name] = n
        setStatusByNode(byNode)
        setStatusError(null)
        if (!editing.current) {
          const latest = await fetchTopology(token)
          if (alive && !editing.current) setTopology((previous) =>
            JSON.stringify(previous) === JSON.stringify(latest) ? previous : latest)
        }
      } catch {
        if (alive) setStatusError('État du pool indisponible — dernières informations conservées')
        // Le hub peut être momentanément injoignable : on garde le dernier
        // état connu plutôt que de faire clignoter toute la mosaïque.
      }
    }
    document.addEventListener('visibilitychange', refresh)
    refresh()
    const timer = setInterval(refresh, 3000)
    return () => {
      alive = false
      clearInterval(timer)
      document.removeEventListener('visibilitychange', refresh)
    }
  }, [token])

  useEffect(() => {
    const onKey = (e) => {
      if (e.target.closest('input, select, textarea, button, [contenteditable=true]')) return
      const mod = e.ctrlKey || e.metaKey
      if (mod && e.key.toLowerCase() === 'z') {
        e.preventDefault()
        e.shiftKey ? redo() : undo()
        return
      }
      if (mod && e.key.toLowerCase() === 'y') {
        e.preventDefault()
        redo()
        return
      }
      if (!selected || !topology?.nodes?.[selected]) return
      const moves = { ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, -1], ArrowDown: [0, 1] }
      const move = moves[e.key]
      if (move) {
        e.preventDefault()
        nudge(selected, move[0], move[1], e.shiftKey)
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  })

  const computedLayout = useMemo(() => scaleLayout(topology?.nodes), [topology])
  const layout = dragId ? dragStart.current[dragId]?.layout || computedLayout : computedLayout

  const lines = useMemo(
    () => connectionLines(topology?.nodes, layout.scale, layout),
    [topology, layout],
  )

  const allIds = useMemo(
    () => Object.keys(topology?.nodes || {}).sort((a, b) => {
      const na = topology.nodes[a]
      const nb = topology.nodes[b]
      return na.x - nb.x || na.y - nb.y
    }),
    [topology],
  )
  // Les machines sans KVM sont garées hors mosaïque par le hub : on les liste
  // à part plutôt que de les dessiner à 100 000 px, ce qui écrasait l'échelle.
  const nodeIds = useMemo(
    () => allIds.filter((id) => !isParked(topology?.nodes?.[id])),
    [allIds, topology],
  )
  const parkedIds = useMemo(
    () => allIds.filter((id) => isParked(topology?.nodes?.[id])),
    [allIds, topology],
  )

  const applyTopology = (nodes) => {
    setTopology((prev) => ({ ...prev, nodes }))
    setSaved(false)
    setDirty(true)
  }

  // Annuler / refaire : un glisser raté se corrigeait à l'œil, en re-glissant.
  const [past, setPast] = useState([])
  const [future, setFuture] = useState([])
  const pushHistory = () => {
    setPast((p) => [...p.slice(-49), topology.nodes])
    setFuture([])
  }
  const undo = () => {
    setPast((p) => {
      if (!p.length) return p
      const previous = p[p.length - 1]
      setFuture((f) => [topology.nodes, ...f])
      setTopology((t) => ({ ...t, nodes: previous }))
      setSaved(false)
      setDirty(true)
      return p.slice(0, -1)
    })
  }
  const redo = () => {
    setFuture((f) => {
      if (!f.length) return f
      const [next, ...rest] = f
      setPast((p) => [...p, topology.nodes])
      setTopology((t) => ({ ...t, nodes: next }))
      setSaved(false)
      setDirty(true)
      return rest
    })
  }

  const updateNode = (id, patch) => {
    pushHistory()
    const nodes = {
      ...topology.nodes,
      [id]: { ...topology.nodes[id], ...patch },
    }
    applyTopology(inferNeighbors({ nodes }).nodes)
  }

  const recalcNeighbors = () => {
    pushHistory()
    const next = inferNeighbors(topology)
    applyTopology(next.nodes)
  }

  /** Position visée pendant/à la fin d'un glisser, aimantée aux bords voisins. */
  const draggedPosition = (id, info) => {
    const start = dragStart.current[id] || topology.nodes[id]
    const [sx, sy] = snapToNeighbors(
      topology.nodes,
      id,
      start.x + info.offset.x / layout.scale,
      start.y + info.offset.y / layout.scale,
    )
    return { x: sx, y: sy }
  }

  // Pendant le glisser, on recalcule les voisins en direct : les liaisons
  // apparaissent quand les bords se touchent, au lieu de n'être découvertes
  // qu'après avoir lâché — et en cas de raté, il n'y a rien à défaire.
  const onDrag = (id, _e, info) => {
    const pos = draggedPosition(id, info)
    const node = topology.nodes[id]
    if (node.x === pos.x && node.y === pos.y) return
    const patched = { ...topology.nodes, [id]: { ...node, ...pos } }
    setTopology((prev) => ({ ...prev, nodes: inferNeighbors({ nodes: patched }).nodes }))
  }

  const onDragEnd = (id, _e, info) => {
    setDragId(null)
    const patched = {
      ...topology.nodes,
      [id]: { ...topology.nodes[id], ...draggedPosition(id, info) },
    }
    applyTopology(inferNeighbors({ nodes: patched }).nodes)
  }

  /** Déplacement au clavier : un cran de grille, dix avec Maj. */
  const nudge = (id, dx, dy, big) => {
    const step = SNAP_GRID_PX * (big ? 10 : 1)
    const n = topology.nodes[id]
    const patched = {
      ...topology.nodes,
      [id]: { ...n, x: n.x + dx * step, y: n.y + dy * step },
    }
    pushHistory()
    applyTopology(inferNeighbors({ nodes: patched }).nodes)
  }

  const handleSave = async () => {
    if (!token.trim()) {
      setError('Token requis pour enregistrer')
      return
    }
    onTokenChange(token.trim())
    if (Object.values(topology.nodes).some((n) => !Number.isInteger(n.width) || !Number.isInteger(n.height) || n.width < 1 || n.height < 1 || n.width > 32768 || n.height > 32768)) {
      setError('Chaque écran doit avoir une résolution entre 1 et 32768 pixels')
      return
    }
    const toSave = inferNeighbors(topology)
    try {
      await saveTopology(toSave, token.trim())
      setTopology(toSave)
      setSaved(true)
      setDirty(false)
      setError(null)
    } catch (err) {
      setError(err.message || 'Échec enregistrement')
    }
  }

  if (!topology) {
    return (
      <div className="mx-auto max-w-6xl p-8 text-slate-500">
        Chargement de la topologie…
      </div>
    )
  }

  return (
    <div className="mesh-bg mx-auto w-full max-w-6xl p-6 md:p-8">
      <PageHeader
        icon={Settings2}
        title="Configuration KVM"
        subtitle="Glissez les écrans comme dans Barrier — les voisins se recalculent automatiquement"
      />

      <div className="mb-4 flex flex-wrap items-end gap-3">
        <label className="flex flex-col gap-1 text-sm">
          <span className="font-semibold text-slate-600">Token hub</span>
          <input
            type="password"
            value={token}
            onChange={(e) => onTokenChange(e.target.value)}
            className="rounded-lg border border-slate-200 px-3 py-2 font-mono text-sm"
            placeholder="POOLSYNC_TOKEN"
            autoComplete="current-password"
          />
        </label>
        <button
          type="button"
          onClick={undo}
          disabled={!past.length || !!dragId}
          title="Annuler le dernier déplacement (Ctrl+Z)"
          className="inline-flex items-center gap-2 rounded-lg border border-slate-200 bg-white px-3 py-2 text-sm font-semibold text-slate-700 shadow-sm transition hover:bg-slate-50 disabled:cursor-not-allowed disabled:opacity-40"
        >
          <Undo2 size={16} />
          Annuler
        </button>
        <button
          type="button"
          onClick={redo}
          disabled={!future.length || !!dragId}
          title="Refaire (Ctrl+Maj+Z)"
          className="inline-flex items-center gap-2 rounded-lg border border-slate-200 bg-white px-3 py-2 text-sm font-semibold text-slate-700 shadow-sm transition hover:bg-slate-50 disabled:cursor-not-allowed disabled:opacity-40"
        >
          <Redo2 size={16} />
          Refaire
        </button>
        <label className="inline-flex items-center gap-2 rounded-lg border border-slate-200 bg-white px-3 py-2 text-sm text-slate-700 shadow-sm">
          <MonitorSmartphone size={16} className="text-emerald-600" />
          Ma machine
          <select
            value={myNode}
            onChange={(e) => {
              setMyNode(e.target.value)
              localStorage.setItem('poolsync_my_node', e.target.value)
            }}
            className="rounded border border-slate-200 bg-white px-1 py-0.5 text-sm"
          >
            <option value="">—</option>
            {allIds.map((id) => (
              <option key={id} value={id}>{nodeLabel(id)}</option>
            ))}
          </select>
        </label>
        <button
          type="button"
          onClick={async () => {
            if (!token.trim()) {
              setError('Token requis pour tester les bords')
              return
            }
            try {
              await showEdges(token.trim(), null)
              setError(null)
            } catch (err) {
              setError(err.message)
            }
          }}
          title="Fait apparaître 3 secondes, sur chaque machine, une bande lumineuse le long des bords reliés à un voisin"
          className="inline-flex items-center gap-2 rounded-lg border border-slate-200 bg-white px-3 py-2 text-sm font-semibold text-slate-700 shadow-sm transition hover:bg-slate-50"
        >
          <Zap size={16} className="text-amber-500" />
          Tester les bords
        </button>
        <button
          type="button"
          onClick={recalcNeighbors}
          className="inline-flex items-center gap-2 rounded-lg border border-slate-200 bg-white px-4 py-2 text-sm font-semibold text-slate-700 shadow-sm hover:bg-slate-50"
        >
          <Link2 size={16} />
          Recalculer voisins
        </button>
        <button
          type="button"
          onClick={handleSave}
          disabled={!dirty || !!dragId}
          className="inline-flex items-center gap-2 rounded-lg bg-indigo-600 px-4 py-2 text-sm font-semibold text-white shadow-sm hover:bg-indigo-700"
        >
          <Save size={16} />
          Enregistrer
        </button>
        {dirty && <span role="status" className="text-sm text-amber-700">Modifications non enregistrées</span>}
        {saved && <span className="text-sm text-emerald-600">Topologie envoyée aux agents</span>}
        {error && <span className="text-sm text-red-600">{error}</span>}
      </div>

      {statusError && <p role="alert" className="mb-3 text-sm text-amber-700">{statusError}</p>}
      <p className="mb-4 text-sm text-slate-600">Pour emporter un ordinateur : clic droit sur son icône PoolSync → « Machine temporairement à l’écart du pool ». Décochez au retour ; sa position est conservée. Le maître suit le clavier ou la souris que vous utilisez.</p>
      <div className="mb-6 rounded-xl border border-slate-200 bg-white p-4 shadow-sm">
        <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
          <div className="flex items-center gap-2 text-sm font-semibold text-slate-700">
            <MousePointer2 size={16} className="text-indigo-600" />
            Mosaïque d&apos;écrans
          </div>
          <span className="text-xs text-slate-400">
            Grille {SNAP_GRID_PX}px · aimantation sur les bords · flèches pour déplacer, Maj pour 10 crans
          </span>
        </div>
        <div
          className="relative overflow-auto rounded-lg border border-dashed border-slate-300 bg-[repeating-linear-gradient(0deg,transparent,transparent_19px,#e2e8f0_19px,#e2e8f0_20px),repeating-linear-gradient(90deg,transparent,transparent_19px,#e2e8f0_19px,#e2e8f0_20px)] bg-slate-50"
          style={{ minHeight: layout.height + 20 }}
        >
          <div className="relative" style={{ width: layout.width, height: layout.height }}>
            <svg
              className="pointer-events-none absolute inset-0"
              width={layout.width}
              height={layout.height}
              aria-hidden
            >
              {lines.map((ln) => (
                <line
                  key={`${ln.a}-${ln.b}`}
                  x1={ln.x1}
                  y1={ln.y1}
                  x2={ln.x2}
                  y2={ln.y2}
                  stroke="#6366f1"
                  strokeWidth={2}
                  strokeDasharray="6 4"
                  opacity={0.7}
                />
              ))}
            </svg>

            {nodeIds.map((id) => {
              const n = topology.nodes[id]
              const rect = nodeRect(n, layout.scale, layout)
              const active = dragId === id
              const status = statusByNode[id]
              const offline = !status || !status.online
              const deaf = status?.online && !status.clipboard_sync
              return (
                <div
                  key={id}
                  role="button"
                  tabIndex={0}
                  aria-label={`Déplacer ${nodeLabel(id)}`}
                  aria-pressed={selected === id}
                  onFocus={() => setSelected(id)}
                  onPointerDown={(event) => {
                    if (event.button !== 0) return
                    event.preventDefault()
                    event.currentTarget.focus()
                    event.currentTarget.setPointerCapture(event.pointerId)
                    pushHistory()
                    setDragId(id); setSelected(id)
                    dragStart.current[id] = { x: n.x, y: n.y, clientX: event.clientX, clientY: event.clientY, layout }
                  }}
                  onPointerMove={(event) => {
                    if (!event.currentTarget.hasPointerCapture(event.pointerId)) return
                    const start = dragStart.current[id]
                    onDrag(id, event, { offset: { x: event.clientX - start.clientX, y: event.clientY - start.clientY } })
                  }}
                  onPointerUp={(event) => {
                    if (!event.currentTarget.hasPointerCapture(event.pointerId)) return
                    event.currentTarget.releasePointerCapture(event.pointerId)
                    const start = dragStart.current[id]
                    onDragEnd(id, event, { offset: { x: event.clientX - start.clientX, y: event.clientY - start.clientY } })
                  }}
                  onPointerCancel={() => setDragId(null)}
                  className={`absolute flex cursor-grab flex-col items-center justify-center overflow-hidden rounded-md border-2 text-center shadow-lg active:cursor-grabbing ${
                    offline
                      ? 'border-slate-300 border-dashed bg-slate-100/70 text-slate-400'
                      : deaf
                        ? 'border-red-400 bg-red-50/90 text-red-900'
                        : n.kvm_enabled
                          ? 'border-indigo-400 bg-indigo-100/90 text-indigo-950'
                          : 'border-slate-400 bg-slate-200/90 text-slate-600'
                  } ${active ? 'z-20 ring-2 ring-indigo-500' : selected === id ? 'z-20 ring-2 ring-sky-400' : 'z-10'} ${
                    myNode === id ? 'outline outline-2 outline-offset-2 outline-emerald-500' : ''
                  }`}
                  style={{
                    touchAction: 'none',
                    left: rect.left,
                    top: rect.top,
                    width: Math.max(rect.width, 72),
                    height: Math.max(rect.height, 48),
                  }}
                  title={`${id} — ${n.width}×${n.height}`}
                >
                  <NodeBadges status={status} />
                  {myNode === id && (
                    <span className="absolute left-1 top-1 rounded bg-emerald-600 px-1 text-[8px] font-bold uppercase text-white">
                      ici
                    </span>
                  )}
                  <LayoutGrid size={14} className="mb-0.5 opacity-40" />
                  <span className="px-1 text-sm font-bold leading-tight">{nodeLabel(id)}</span>
                  <span className="font-mono text-[10px] opacity-60">{n.width}×{n.height}</span>
                  <MonitorMap
                    monitors={status?.monitors}
                    poolMonitor={{ x: n.monitor_x, y: n.monitor_y }}
                  />
                  <LastClip clip={status?.last_clip} />
                  {!n.kvm_enabled && (
                    <span className="mt-0.5 text-[9px] font-semibold uppercase tracking-wide text-slate-500">
                      clip only
                    </span>
                  )}
                </div>
              )
            })}
          </div>
        </div>
        <p className="mt-2 text-xs text-slate-500">
          Position ({CANVAS_PAD}px marge) :{' '}
          {nodeIds.map((id) => {
            const n = topology.nodes[id]
            return `${nodeLabel(id)} (${n.x}, ${n.y})`
          }).join(' · ')}
        </p>

        {parkedIds.length > 0 && (
          <div className="mt-4 border-t border-slate-200 pt-3">
            <p className="mb-2 text-xs font-semibold uppercase tracking-wide text-slate-500">
              Presse-papiers seul — hors mosaïque KVM
            </p>
            <div className="flex flex-wrap gap-2">
              {parkedIds.map((id) => {
                const status = statusByNode[id]
                return (
                  <div
                    key={id}
                    className={`relative flex min-w-[7rem] flex-col rounded-md border px-2 py-1.5 text-xs ${
                      !status || !status.online
                        ? 'border-dashed border-slate-300 bg-slate-50 text-slate-400'
                        : status.clipboard_sync
                          ? 'border-slate-300 bg-white text-slate-700'
                          : 'border-red-300 bg-red-50 text-red-900'
                    }`}
                    title={`${id} — ${topology.nodes[id].width}×${topology.nodes[id].height}`}
                  >
                    <NodeBadges status={status} />
                    <span className="font-semibold">{nodeLabel(id)}</span>
                    <span className="font-mono text-[10px] opacity-60">
                      {topology.nodes[id].width}×{topology.nodes[id].height}
                    </span>
                    <MonitorMap
                      monitors={status?.monitors}
                      poolMonitor={{
                        x: topology.nodes[id].monitor_x,
                        y: topology.nodes[id].monitor_y,
                      }}
                    />
                    <LastClip clip={status?.last_clip} />
                  </div>
                )
              })}
            </div>
          </div>
        )}
      </div>

      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
        {nodeIds.filter((id) => id === selected).map((id) => {
          const n = topology.nodes[id]
          return (
            <div key={id} className="rounded-xl border border-slate-200 bg-white p-4 shadow-sm">
              <div className="mb-3 flex items-center justify-between">
                <h3 className="font-bold text-slate-800">{nodeLabel(id)}</h3>
                <label className="flex items-center gap-2 text-sm">
                  <input
                    type="checkbox"
                    checked={n.kvm_enabled}
                    onChange={(e) => updateNode(id, { kvm_enabled: e.target.checked })}
                  />
                  KVM actif
                </label>
              </div>
              <div className="mb-3 grid grid-cols-2 gap-2 text-sm">
                <label className="flex flex-col gap-1">
                  Largeur
                  <input
                    type="number"
                    min={1}
                    max={32768}
                    value={n.width}
                    onChange={(e) => updateNode(id, { width: Number(e.target.value) })}
                    className="rounded border border-slate-200 px-2 py-1"
                  />
                </label>
                <label className="flex flex-col gap-1">
                  Hauteur
                  <input
                    type="number"
                    min={1}
                    max={32768}
                    value={n.height}
                    onChange={(e) => updateNode(id, { height: Number(e.target.value) })}
                    className="rounded border border-slate-200 px-2 py-1"
                  />
                </label>
              </div>
              <p className="text-sm text-slate-600">Voisins calculés : {DIRS.map((dir) => `${DIR_LABEL[dir]} ${n.neighbors?.[dir] || '—'}`).join(' · ')}</p>
              <p className="mt-2 text-xs text-slate-500">Choisissez une vignette pour régler cette machine. Les coordonnées sont en pixels, même si les écrans ont des résolutions différentes. Les moniteurs supplémentaires restent accessibles localement ; le passage entre machines utilise le moniteur principal.</p>
            </div>
          )
        })}
      </div>
    </div>
  )
}
