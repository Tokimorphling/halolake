/*
Copyright (C) 2023-2026 QuantumNous

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU Affero General Public License as
published by the Free Software Foundation, either version 3 of the
License, or (at your option) any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
GNU Affero General Public License for more details.

You should have received a copy of the GNU Affero General Public License
along with this program. If not, see <https://www.gnu.org/licenses/>.

For commercial licensing, please contact support@quantumnous.com
*/
import { Link } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'

import { Skeleton } from '@/components/ui/skeleton'
import { useSystemConfig } from '@/hooks/use-system-config'
import { cn } from '@/lib/utils'

type AuthLayoutProps = {
  children: React.ReactNode
}

export function AuthLayout({ children }: AuthLayoutProps) {
  const { t } = useTranslation()
  const { systemName, logo, loading } = useSystemConfig()

  return (
    <div className='bg-background text-foreground relative grid min-h-svh max-w-none'>
      <Link
        to='/'
        className={cn(
          'absolute top-4 left-4 z-10 flex items-center gap-2.5 rounded-full px-2.5 py-1.5 transition-opacity hover:opacity-90 sm:top-6 sm:left-6',
          'app-material-chrome border border-[color:var(--material-chrome-border)]',
          'shadow-[0_1px_0_0_var(--hairline)]'
        )}
      >
        <div className='relative size-7 shrink-0'>
          {loading ? (
            <Skeleton className='absolute inset-0 rounded-lg' />
          ) : (
            <img
              src={logo}
              alt={t('Logo')}
              className='size-7 rounded-lg object-contain'
            />
          )}
        </div>
        {loading ? (
          <Skeleton className='h-4 w-20' />
        ) : (
          <span className='text-sm font-semibold tracking-tight'>
            {systemName}
          </span>
        )}
      </Link>
      <div className='container flex items-center justify-center px-4 py-20 sm:py-10'>
        <div
          className={cn(
            'bg-card/90 mx-auto flex w-full flex-col justify-center space-y-2 px-5 py-7 sm:w-[480px] sm:px-8 sm:py-9',
            'rounded-2xl ring-1 ring-foreground/6',
            'shadow-[0_8px_40px_oklch(0_0_0/0.08),0_1px_0_0_var(--hairline)]',
            'dark:bg-card/80 dark:shadow-[0_12px_48px_oklch(0_0_0/0.35),0_1px_0_0_var(--hairline)]',
            'backdrop-blur-md'
          )}
        >
          {children}
        </div>
      </div>
    </div>
  )
}
