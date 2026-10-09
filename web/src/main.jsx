import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import './index.css'
import App from './App.jsx'

const savedTheme = localStorage.getItem('poolsync-theme') === 'dark' ? 'dark' : 'light'
document.documentElement.classList.toggle('theme-dark', savedTheme === 'dark')
document.documentElement.classList.toggle('theme-light', savedTheme === 'light')

createRoot(document.getElementById('root')).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
