import { Clipboard, ClipboardX, Crown, PauseCircle, WifiOff } from 'lucide-react'

/**
 * Pastilles d'état posées sur une carte de la mosaïque.
 *
 * On ne signale que ce qui sort de la normale : un nœud sain n'affiche rien,
 * pour que l'anomalie saute aux yeux. C'est ce qui manquait le 02/09, quand un
 * nœud à la synchro coupée ressemblait trait pour trait à un nœud sain.
 */
export default function NodeBadges({ status }) {
  if (!status) {
    return (
      <Badge title="Ce nœud n'est pas connecté au hub" tone="slate">
        <WifiOff size={11} /> hors ligne
      </Badge>
    )
  }
  return (
    <span className="pointer-events-none absolute right-1 top-1 flex flex-col items-end gap-0.5">
      {!status.online && (
        <Badge title="Ce nœud n'est pas connecté au hub" tone="slate">
          <WifiOff size={11} /> hors ligne
        </Badge>
      )}
      {status.online && !status.clipboard_sync && (
        <Badge title="Synchro presse-papiers coupée : ce nœud ne réplique rien" tone="red">
          <ClipboardX size={11} /> synchro coupée
        </Badge>
      )}
      {status.online && !status.local_active && (
        <Badge title="PoolSync est en pause sur ce poste (Ctrl+Alt+Shift+P)" tone="amber">
          <PauseCircle size={11} /> en pause
        </Badge>
      )}
      {status.is_master && (
        <Badge title="Ce nœud possède le clavier et la souris" tone="indigo">
          <Crown size={11} /> maître
        </Badge>
      )}
    </span>
  )
}

const TONES = {
  slate: 'bg-slate-600 text-white',
  red: 'bg-red-600 text-white',
  amber: 'bg-amber-500 text-white',
  indigo: 'bg-indigo-600 text-white',
}

function Badge({ children, title, tone }) {
  return (
    <span
      title={title}
      className={`flex items-center gap-1 rounded px-1 py-px text-[9px] font-semibold shadow-sm ${TONES[tone]}`}
    >
      {children}
    </span>
  )
}

/** Dernière copie d'un nœud, affichée sous son nom. */
export function LastClip({ clip }) {
  if (!clip) return null
  const preview = clip.is_image ? 'image' : clip.preview
  return (
    <span
      className="mt-0.5 flex max-w-full items-center gap-1 truncate px-1 text-[9px] opacity-70"
      title={`Dernière copie : ${clip.preview}`}
    >
      <Clipboard size={9} className="shrink-0" />
      <span className="truncate">{preview}</span>
    </span>
  )
}
