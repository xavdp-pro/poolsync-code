import { motion } from 'framer-motion'
import {
  ClipboardCopy,
  Crown,
  LayoutDashboard,
  Monitor,
  RefreshCw,
  Server,
  Wifi,
  WifiOff,
} from 'lucide-react'
import { useCallback, useEffect, useState } from 'react'
import PageHeader from '../components/PageHeader'
import { fetchStatus, formatTs, modeLabel, nodeLabel, shortHash } from '../api'

function StatCard({ icon: Icon, label, value, hint, accent = 'indigo' }) {
  const accentMap = {
    indigo: 'bg-indigo-50 text-indigo-600',
    emerald: 'bg-emerald-50 text-emerald-600',
    amber: 'bg-amber-50 text-amber-600',
    slate: 'bg-slate-100 text-slate-600',
  }
  return (
    <motion.div
      initial={{ opacity: 0, y: 10 }}
      animate={{ opacity: 1, y: 0 }}
      className="rounded-xl border border-slate-200 bg-white p-4 shadow-sm"
    >
      <div className="flex items-start gap-3">
        <span className={`flex h-9 w-9 shrink-0 items-center justify-center rounded-lg ${accentMap[accent]}`}>
          <Icon size={18} />
        </span>
        <div className="min-w-0">
          <div className="text-xs font-semibold uppercase tracking-wide text-slate-400">{label}</div>
          <div className="mt-0.5 truncate text-lg font-bold text-slate-800">{value}</div>
          {hint && <div className="mt-0.5 text-xs text-slate-500">{hint}</div>}
        </div>
      </div>
    </motion.div>
  )
}

function NodeCard({ node, index }) {
  const online = Boolean(node?.online)
  const isMaster = Boolean(node?.is_master)

  return (
    <motion.div
      initial={{ opacity: 0, y: 12 }}
      animate={{ opacity: 1, y: 0 }}
      transition={{ delay: index * 0.06 }}
      className={`rounded-xl border p-5 shadow-sm ${
        online ? 'border-slate-200 bg-white' : 'border-dashed border-slate-300 bg-slate-50/80'
      }`}
    >
      <div className="flex items-start justify-between gap-2">
        <div className="flex items-center gap-2">
          <span className={`h-2.5 w-2.5 rounded-full ${online ? 'bg-emerald-500' : 'bg-slate-300'}`} />
          <h3 className="text-base font-bold text-slate-800">{nodeLabel(node.name)}</h3>
          {isMaster && (
            <span className="inline-flex items-center gap-1 rounded-full bg-amber-50 px-2 py-0.5 text-[11px] font-semibold text-amber-700">
              <Crown size={12} /> maître
            </span>
          )}
        </div>
        {online ? (
          <Wifi size={16} className="text-emerald-500" />
        ) : (
          <WifiOff size={16} className="text-slate-400" />
        )}
      </div>

      <div className="mt-4 space-y-2 text-sm">
        <div className="flex justify-between gap-2">
          <span className="text-slate-400">Statut</span>
          <span className={online ? 'font-semibold text-emerald-600' : 'text-slate-400'}>
            {online ? 'Connecté' : 'Hors ligne'}
          </span>
        </div>
        {/* Un nœud connecté mais dont la synchro est coupée, ou en pause, ne
            réplique rien : c'est l'anomalie la plus coûteuse à ne pas voir. */}
        <div className="flex justify-between gap-2">
          <span className="text-slate-400">Presse-papiers</span>
          {!online ? (
            <span className="text-slate-400">—</span>
          ) : node.clipboard_sync === false ? (
            <span
              className="font-semibold text-red-600"
              title="La case « Presse-papiers PoolSync » est décochée dans le menu systray de ce poste"
            >
              synchro coupée
            </span>
          ) : node.local_active === false ? (
            <span
              className="font-semibold text-amber-600"
              title="PoolSync est suspendu sur ce poste (Ctrl+Alt+Shift+P)"
            >
              en pause
            </span>
          ) : (
            <span className="font-semibold text-emerald-600">synchronisé</span>
          )}
        </div>
        <div className="flex justify-between gap-2">
          <span className="text-slate-400">Mode</span>
          <span className="text-slate-600">{modeLabel(node.mode)}</span>
        </div>
        <div className="flex justify-between gap-2">
          <span className="text-slate-400">KVM</span>
          <span className={node.kvm_enabled ? 'font-semibold text-indigo-600' : 'text-slate-400'}>
            {node.kvm_enabled ? 'Actif' : 'Désactivé'}
          </span>
        </div>
        <div className="flex justify-between gap-2">
          <span className="text-slate-400">
            {(node.monitors?.length || 0) > 1 ? `Écrans (${node.monitors.length})` : 'Écran'}
          </span>
          <span className="text-right font-mono text-slate-600">
            {(node.monitors?.length || 0) > 1 ? (
              node.monitors.map((m) => (
                <span key={`${m.name}-${m.x}`} className="block text-[11px] leading-tight">
                  {m.name || '?'} {m.width}×{m.height}
                  {m.primary && <span className="ml-1 text-indigo-500">principal</span>}
                </span>
              ))
            ) : (
              <>{node.screen?.width}×{node.screen?.height}</>
            )}
          </span>
        </div>
        <div className="flex justify-between gap-2">
          <span className="text-slate-400">Depuis</span>
          <span className="text-slate-600">{formatTs(node.connected_at)}</span>
        </div>
        {node.last_clip && (
          <div className="flex justify-between gap-2">
            <span className="shrink-0 text-slate-400">Dernière copie</span>
            <span
              className="truncate text-right text-slate-600"
              title={`${node.last_clip.preview} — ${formatTs(node.last_clip.at)}`}
            >
              {node.last_clip.is_image ? 'image' : node.last_clip.preview}
            </span>
          </div>
        )}
      </div>
    </motion.div>
  )
}

function TopologyBar({ nodes, master }) {
  if (!nodes.length) {
    return (
      <div className="rounded-xl border border-slate-200 bg-white p-5 text-center text-sm text-slate-500 shadow-sm">
        Aucun nœud connecté pour l'instant.
      </div>
    )
  }
  return (
    <div className="rounded-xl border border-slate-200 bg-white p-5 shadow-sm">
      <div className="mb-4 flex items-center gap-2">
        <Monitor size={18} className="text-indigo-600" />
        <h2 className="text-base font-bold text-slate-800">Topologie pool</h2>
      </div>
      <div className="flex flex-wrap items-center justify-center gap-2 sm:gap-3">
        {nodes.map((node, i) => {
          const online = Boolean(node?.online)
          const isMaster = master === node.name
          return (
            <div key={node.name} className="flex items-center gap-2 sm:gap-3">
              <div
                className={`min-w-[5.5rem] rounded-lg border px-3 py-2 text-center text-sm font-semibold sm:min-w-[6.5rem] ${
                  online
                    ? isMaster
                      ? 'border-amber-300 bg-amber-50 text-amber-800'
                      : 'border-emerald-200 bg-emerald-50 text-emerald-800'
                    : 'border-slate-200 bg-slate-50 text-slate-400'
                }`}
              >
                {nodeLabel(node.name)}
              </div>
              {i < nodes.length - 1 && <span className="text-slate-300">→</span>}
            </div>
          )
        })}
      </div>
    </div>
  )
}

export default function Dashboard({ token, onTokenChange }) {
  const [data, setData] = useState(null)
  const [error, setError] = useState(null)
  const [loading, setLoading] = useState(true)
  const [lastRefresh, setLastRefresh] = useState(null)

  const refresh = useCallback(async () => {
    if (document.hidden) return
    try {
      const json = await fetchStatus(token)
      setData(json)
      setError(null)
      setLastRefresh(new Date())
    } catch (err) {
      setError(err.message || 'Erreur réseau')
    } finally {
      setLoading(false)
    }
  }, [token])

  useEffect(() => {
    refresh()
    const id = setInterval(refresh, 3000)
    document.addEventListener('visibilitychange', refresh)
    return () => { clearInterval(id); document.removeEventListener('visibilitychange', refresh) }
  }, [refresh])

  const nodes = [...(data?.nodes || [])].sort((a, b) => a.name.localeCompare(b.name))
  const onlineCount = data?.hub?.node_count ?? nodes.length
  const master = data?.master

  return (
    <div className="mesh-bg mx-auto w-full max-w-6xl p-6 md:p-8">
      <PageHeader
        icon={LayoutDashboard}
        title="PoolSync"
        subtitle="Presse-papiers + KVM · maître dynamique"
      />

      <div className="mb-4 flex flex-wrap items-center gap-2">
        <label className="flex items-center gap-2 rounded-lg border border-slate-200 bg-white px-3 py-2 text-sm shadow-sm">
          <span className="font-semibold text-slate-600">Token hub</span>
          <input
            type="password"
            value={token}
            onChange={(event) => onTokenChange(event.target.value)}
            className="w-36 bg-transparent font-mono text-sm outline-none"
            placeholder="POOLSYNC_TOKEN"
            autoComplete="current-password"
          />
        </label>
        <button
          type="button"
          onClick={refresh}
          className="inline-flex items-center gap-2 rounded-lg border border-slate-200 bg-white px-3 py-2 text-sm font-semibold text-slate-700 shadow-sm transition-colors hover:border-indigo-200 hover:text-indigo-700"
        >
          <RefreshCw size={15} className={loading ? 'animate-spin' : ''} />
          Actualiser
        </button>
        {lastRefresh && (
          <span className="text-xs text-slate-400">
            MAJ {lastRefresh.toLocaleTimeString('fr-FR')}
          </span>
        )}
        {error && (
          <span className="rounded-md bg-red-50 px-2 py-1 text-xs font-medium text-red-700">
            {error}
          </span>
        )}
      </div>

      <div className="mb-6 grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <StatCard
          icon={Server}
          label="Hub"
          value={error ? 'Injoignable' : 'En ligne'}
          hint={data ? `v${data.hub.version} · démarré ${formatTs(data.hub.started_at)}` : '…'}
          accent={error ? 'slate' : 'emerald'}
        />
        <StatCard
          icon={Wifi}
          label="Nœuds connectés"
          value={onlineCount}
          hint={nodes.length ? nodes.map((n) => n.name).join(' · ') : 'Aucun'}
          accent="indigo"
        />
        <StatCard
          icon={Crown}
          label="Maître KVM"
          value={master ? nodeLabel(master) : '—'}
          hint="Machine active"
          accent="amber"
        />
        <StatCard
          icon={ClipboardCopy}
          label="Dernier clip"
          value={shortHash(data?.clipboard?.last_hash)}
          hint={data?.clipboard?.last_at ? formatTs(data.clipboard.last_at) : 'Aucune sync récente'}
          accent="indigo"
        />
      </div>

      <div className="mb-6">
        <TopologyBar nodes={nodes} master={master} />
      </div>

      <div className="grid grid-cols-1 gap-4 md:grid-cols-2 xl:grid-cols-3">
        {nodes.map((node, i) => (
          <NodeCard key={node.name} node={node} index={i} />
        ))}
      </div>
    </div>
  )
}
