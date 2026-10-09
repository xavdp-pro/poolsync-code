import Dashboard from './pages/Dashboard'
import Config from './pages/Config'
import ThemeToggle from './components/ThemeToggle'
import { useState } from 'react'

export default function App() {
  const [tab, setTab] = useState('dashboard')
  const [token, setToken] = useState(() => localStorage.getItem('poolsync_token') || '')

  const updateToken = (value) => {
    setToken(value)
    if (value.trim()) localStorage.setItem('poolsync_token', value.trim())
    else localStorage.removeItem('poolsync_token')
  }

  return (
    <div className="relative min-h-[100dvh] bg-slate-50">
      <nav className="sticky top-0 z-20 border-b border-slate-200 bg-white/90 backdrop-blur">
        <div className="mx-auto flex max-w-6xl gap-1 px-6 py-2">
          {[
            ['dashboard', 'Tableau de bord'],
            ['config', 'Config KVM'],
          ].map(([id, label]) => (
            <button
              key={id}
              type="button"
              onClick={() => setTab(id)}
              className={`rounded-lg px-3 py-2 text-sm font-semibold ${
                tab === id
                  ? 'bg-indigo-50 text-indigo-700'
                  : 'text-slate-600 hover:bg-slate-100'
              }`}
            >
              {label}
            </button>
          ))}
        </div>
      </nav>
      {tab === 'dashboard' ? (
        <Dashboard token={token} onTokenChange={updateToken} />
      ) : (
        <Config token={token} onTokenChange={updateToken} />
      )}
      <ThemeToggle />
    </div>
  )
}
