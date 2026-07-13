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
import { createElement, type ReactNode } from 'react'

import { cn } from '@/lib/utils'

export const sideDrawerContentClassName = (className?: string) =>
  cn(
    'bg-background text-foreground flex h-dvh w-full flex-col gap-0 overflow-hidden p-0 shadow-none',
    className
  )

export const sideDrawerHeaderClassName = (className?: string) =>
  cn(
    'app-material-chrome shrink-0 border-b border-[color:var(--material-chrome-border)] px-4 py-3.5 text-start sm:px-6 sm:py-4',
    className
  )

export const sideDrawerFormClassName = (className?: string) =>
  cn(
    'flex min-h-0 flex-1 flex-col gap-5 overflow-y-auto overscroll-contain px-4 py-4 sm:gap-6 sm:px-6 sm:py-5',
    className
  )

export const sideDrawerFooterClassName = (className?: string) =>
  cn(
    'app-material-chrome shrink-0 grid grid-cols-2 gap-2 border-t border-[color:var(--material-chrome-border)] px-4 py-3 sm:flex sm:flex-row sm:justify-end sm:gap-2.5 sm:px-6 sm:py-3.5',
    className
  )

/** Primary form section — elevated card surface for long editors. */
export const sideDrawerSectionClassName = (className?: string) =>
  cn(
    'bg-card/70 ring-foreground/6 flex flex-col gap-4 rounded-2xl p-4 shadow-[0_1px_0_0_var(--hairline)] ring-1 sm:p-5',
    className
  )

/** Nested subsection inside a primary section (no extra outer chrome). */
export const sideDrawerNestedSectionClassName = (className?: string) =>
  cn(
    'border-border/50 flex flex-col gap-3 border-t pt-4 first:border-t-0 first:pt-0',
    className
  )

export const sideDrawerSwitchItemClassName = (className?: string) =>
  cn(
    'border-border/50 bg-muted/20 flex min-h-14 flex-row items-center justify-between gap-3 rounded-xl border px-3 py-3',
    className
  )

export const sideDrawerAlertClassName = (className?: string) =>
  cn('mx-4 shrink-0 sm:mx-6', className)

export const sideDrawerBodyGridClassName = (className?: string) =>
  cn(
    'grid gap-5 lg:grid-cols-[14rem_minmax(0,1fr)] lg:items-start xl:grid-cols-[15rem_minmax(0,1fr)]',
    className
  )

export function SideDrawerSection(props: {
  children: ReactNode
  className?: string
}) {
  return createElement(
    'section',
    { className: sideDrawerSectionClassName(props.className) },
    props.children
  )
}

export function SideDrawerSectionHeader(props: {
  title: ReactNode
  description?: ReactNode
  icon?: ReactNode
  className?: string
}) {
  return createElement(
    'div',
    {
      className: cn(
        'flex items-start gap-3 border-b border-[color:var(--hairline)] pb-3.5',
        props.className
      ),
    },
    props.icon
      ? createElement(
          'span',
          {
            className:
              'bg-muted/80 text-muted-foreground ring-foreground/6 flex size-8 shrink-0 items-center justify-center rounded-lg ring-1',
          },
          props.icon
        )
      : null,
    createElement(
      'div',
      { className: 'min-w-0 flex-1' },
      createElement(
        'h3',
        {
          className:
            'text-title text-sm leading-none font-semibold tracking-[var(--tracking-title)]',
        },
        props.title
      ),
      props.description
        ? createElement(
            'p',
            {
              className:
                'text-muted-foreground mt-1.5 text-xs leading-relaxed',
            },
            props.description
          )
        : null
    )
  )
}
