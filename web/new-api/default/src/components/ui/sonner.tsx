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
'use client'

import {
  CheckmarkCircle02Icon,
  InformationCircleIcon,
  Alert02Icon,
  MultiplicationSignCircleIcon,
  Loading03Icon,
} from '@hugeicons/core-free-icons'
import { HugeiconsIcon } from '@hugeicons/react'
import { Toaster as Sonner, type ToasterProps } from 'sonner'

import { useTheme } from '@/context/theme-provider'

const Toaster = (props: ToasterProps) => {
  const { resolvedTheme } = useTheme()

  return (
    <Sonner
      theme={resolvedTheme}
      className='toaster group'
      icons={{
        success: (
          <HugeiconsIcon
            icon={CheckmarkCircle02Icon}
            strokeWidth={2}
            className='size-4'
          />
        ),
        info: (
          <HugeiconsIcon
            icon={InformationCircleIcon}
            strokeWidth={2}
            className='size-4'
          />
        ),
        warning: (
          <HugeiconsIcon
            icon={Alert02Icon}
            strokeWidth={2}
            className='size-4'
          />
        ),
        error: (
          <HugeiconsIcon
            icon={MultiplicationSignCircleIcon}
            strokeWidth={2}
            className='size-4'
          />
        ),
        loading: (
          <HugeiconsIcon
            icon={Loading03Icon}
            strokeWidth={2}
            className='size-4 animate-spin'
          />
        ),
      }}
      toastOptions={{
        classNames: {
          toast:
            'app-material-chrome border-[color:var(--material-chrome-border)] shadow-[0_8px_32px_-8px_oklch(0_0_0/0.16),0_0_0_0.5px_var(--hairline)] dark:shadow-[0_10px_36px_-10px_oklch(0_0_0/0.5),0_0_0_0.5px_var(--hairline)]',
        },
      }}
      style={
        {
          '--normal-bg': 'var(--material-chrome)',
          '--normal-text': 'var(--popover-foreground)',
          '--normal-border': 'var(--material-chrome-border)',
          '--success-bg':
            'color-mix(in oklch, var(--success) 12%, var(--material-chrome))',
          '--success-border':
            'color-mix(in oklch, var(--success) 28%, var(--material-chrome-border))',
          '--success-text': 'var(--success)',
          '--info-bg':
            'color-mix(in oklch, var(--info) 12%, var(--material-chrome))',
          '--info-border':
            'color-mix(in oklch, var(--info) 28%, var(--material-chrome-border))',
          '--info-text': 'var(--info)',
          '--warning-bg':
            'color-mix(in oklch, var(--warning) 14%, var(--material-chrome))',
          '--warning-border':
            'color-mix(in oklch, var(--warning) 30%, var(--material-chrome-border))',
          '--warning-text': 'var(--warning)',
          '--error-bg':
            'color-mix(in oklch, var(--destructive) 12%, var(--material-chrome))',
          '--error-border':
            'color-mix(in oklch, var(--destructive) 28%, var(--material-chrome-border))',
          '--error-text': 'var(--destructive)',
          '--border-radius': '1rem',
        } as React.CSSProperties
      }
      {...props}
    />
  )
}

export { Toaster }
