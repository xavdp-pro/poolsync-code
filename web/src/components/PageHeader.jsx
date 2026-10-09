import { motion } from 'framer-motion'

export default function PageHeader({ icon: Icon, title, subtitle }) {
  return (
    <motion.div
      initial={{ opacity: 0, y: 8 }}
      animate={{ opacity: 1, y: 0 }}
      transition={{ duration: 0.25 }}
      className="mb-4 flex items-start gap-3 sm:mb-6"
    >
      {Icon && (
        <span className="mt-0.5 flex h-9 w-9 shrink-0 items-center justify-center rounded-xl bg-indigo-50 text-indigo-600 sm:h-10 sm:w-10">
          <Icon size={20} className="sm:hidden" />
          <Icon size={22} className="hidden sm:block" />
        </span>
      )}
      <div className="min-w-0">
        <h1 className="text-lg font-black tracking-tight text-slate-800 sm:text-xl">{title}</h1>
        {subtitle && <p className="mt-0.5 text-sm leading-snug text-slate-500">{subtitle}</p>}
      </div>
    </motion.div>
  )
}
