import { Moon, Sun } from 'lucide-react'
import { useEffect, useState } from 'react'

const storageKey = 'poolsync-theme'

function applyTheme(theme) {
  document.documentElement.classList.toggle('theme-dark', theme === 'dark')
  document.documentElement.classList.toggle('theme-light', theme === 'light')
}

export default function ThemeToggle() {
  const [theme, setTheme] = useState(() => (localStorage.getItem(storageKey) === 'dark' ? 'dark' : 'light'))

  useEffect(() => {
    applyTheme(theme)
    localStorage.setItem(storageKey, theme)
  }, [theme])

  const dark = theme === 'dark'

  return (
    <button
      type="button"
      onClick={() => setTheme(dark ? 'light' : 'dark')}
      className="theme-toggle fixed z-[70] inline-flex h-11 w-11 items-center justify-center rounded-full border border-slate-200 bg-white text-slate-600 shadow-lg transition-colors hover:bg-slate-50 hover:text-indigo-600 max-lg:bottom-[max(0.75rem,env(safe-area-inset-bottom))] max-lg:right-[max(0.75rem,env(safe-area-inset-right))] lg:bottom-3 lg:right-3"
      title={dark ? 'Theme clair' : 'Theme sombre'}
      aria-label={dark ? 'Activer le theme clair' : 'Activer le theme sombre'}
    >
      {dark ? <Sun size={18} /> : <Moon size={18} />}
    </button>
  )
}
