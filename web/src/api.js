function authHeaders(token, headers = {}) {
  const value = token?.trim()
  return value ? { ...headers, Authorization: `Bearer ${value}` } : headers
}

export async function fetchStatus(token) {
  if (!token?.trim()) throw new Error('Token requis — renseignez-le dans le tableau de bord')
  const res = await fetch('/api/status', {
    cache: 'no-store',
    headers: authHeaders(token),
  })
  if (!res.ok) throw new Error(`HTTP ${res.status}`)
  return res.json()
}

export async function fetchTopology(token) {
  const res = await fetch('/api/topology', {
    cache: 'no-store',
    headers: authHeaders(token),
  })
  if (!res.ok) throw new Error(`HTTP ${res.status}`)
  return res.json()
}

export async function saveTopology(topology, token) {
  const res = await fetch('/api/topology', {
    method: 'POST',
    headers: authHeaders(token, { 'Content-Type': 'application/json' }),
    body: JSON.stringify(topology),
  })
  if (!res.ok) throw new Error(`HTTP ${res.status}`)
}

// Libellé lisible dérivé du nom de nœud (générique, aucune machine codée en dur).
export function nodeLabel(name) {
  if (!name) return '—'
  return name.charAt(0).toUpperCase() + name.slice(1)
}

export function formatTs(ts) {
  if (!ts) return '—'
  return new Date(ts * 1000).toLocaleString('fr-FR', {
    day: '2-digit',
    month: 'short',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
  })
}

export function modeLabel(mode) {
  if (mode === 'clipboard_only') return 'Presse-papiers'
  if (mode === 'full') return 'Complet (clip + KVM)'
  return mode
}

export function shortHash(hash) {
  if (!hash) return '—'
  return `${hash.slice(0, 8)}…${hash.slice(-6)}`
}

/**
 * Demande aux agents d'afficher leurs bords KVM à l'écran.
 * `node` absent = tout le pool.
 */
export async function showEdges(token, node) {
  const res = await fetch('/api/edges/show', {
    method: 'POST',
    headers: authHeaders(token, { 'Content-Type': 'application/json' }),
    body: JSON.stringify({ node: node || null, duration_ms: 3000 }),
  })
  if (!res.ok) throw new Error(`Bords : HTTP ${res.status}`)
  return true
}
