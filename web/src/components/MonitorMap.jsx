/**
 * Petit plan des écrans d'une machine, dessiné à l'échelle dans sa carte.
 *
 * La mosaïque ne place qu'un rectangle par machine — celui de l'écran « pool »,
 * celui où le KVM fait basculer la souris. Une machine à deux écrans y était
 * donc représentée comme si elle n'en avait qu'un, et rien ne disait lequel.
 * Ce plan montre la disposition réelle et désigne l'écran retenu.
 */
export default function MonitorMap({ monitors, poolMonitor, width = 96, height = 34 }) {
  if (!monitors || monitors.length < 2) return null

  const minX = Math.min(...monitors.map((m) => m.x))
  const minY = Math.min(...monitors.map((m) => m.y))
  const maxX = Math.max(...monitors.map((m) => m.x + m.width))
  const maxY = Math.max(...monitors.map((m) => m.y + m.height))
  const scale = Math.min(width / Math.max(maxX - minX, 1), height / Math.max(maxY - minY, 1))
  const noPrimary = !monitors.some((m) => m.primary)

  return (
    <span
      className="mt-1 block"
      title={
        noPrimary
          ? "Aucun écran principal déclaré : PoolSync retient le plus grand, qui n'est pas forcément celui où vous travaillez (xrandr --output <sortie> --primary)"
          : monitors.map((m) => `${m.name || '?'} ${m.width}×${m.height}`).join(' · ')
      }
    >
      <svg width={width} height={height} className="overflow-visible">
        {monitors.map((m) => {
          const isPool =
            poolMonitor && m.x === poolMonitor.x && m.y === poolMonitor.y
          return (
            <g key={`${m.name}-${m.x}-${m.y}`}>
              <rect
                x={(m.x - minX) * scale}
                y={(m.y - minY) * scale}
                width={Math.max(m.width * scale - 1, 2)}
                height={Math.max(m.height * scale - 1, 2)}
                rx={1.5}
                className={
                  m.primary || isPool
                    ? 'fill-indigo-500/80 stroke-indigo-700'
                    : 'fill-slate-300/70 stroke-slate-400'
                }
                strokeWidth={0.75}
              />
            </g>
          )
        })}
      </svg>
      {noPrimary && (
        <span className="text-[8px] font-semibold uppercase text-amber-600">
          aucun principal
        </span>
      )}
    </span>
  )
}
