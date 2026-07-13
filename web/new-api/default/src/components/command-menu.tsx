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
import { useLocation, useNavigate } from '@tanstack/react-router'
import { ArrowRight, ChevronRight, Laptop, Moon, Sun } from 'lucide-react'
import React from 'react'
import { useTranslation } from 'react-i18next'

import {
  Command,
  CommandDialog,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
  CommandSeparator,
} from '@/components/ui/command'
import { useSearch } from '@/context/search-provider'
import { useTheme } from '@/context/theme-provider'
import { useSidebarData } from '@/hooks/use-sidebar-data'

import { getNavGroupsForPath } from './layout/lib/sidebar-view-registry'
import { ScrollArea } from './ui/scroll-area'

export function CommandMenu() {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const { setTheme } = useTheme()
  const { open, setOpen } = useSearch()
  const { pathname } = useLocation()
  const sidebarData = useSidebarData()

  // Use the active nested sidebar view's nav groups when one matches
  // the current URL; otherwise fall back to the root navigation.
  const navGroups = getNavGroupsForPath(pathname, t) ?? sidebarData.navGroups

  const runCommand = React.useCallback(
    (command: () => unknown) => {
      setOpen(false)
      command()
    },
    [setOpen]
  )

  return (
    <CommandDialog
      modal
      open={open}
      onOpenChange={setOpen}
      className='app-header-surface top-[18%] max-w-lg overflow-hidden rounded-2xl! p-0 ring-0 sm:max-w-lg'
    >
      <Command className='bg-transparent'>
        <CommandInput placeholder={t('Type a command or search...')} />
        <CommandList>
          <ScrollArea className='h-72 pe-1'>
            <CommandEmpty>{t('No results found.')}</CommandEmpty>
            {navGroups.map((group) => (
              <CommandGroup
                key={group.id || group.title}
                heading={group.title}
                className='p-1.5'
              >
                {group.items.map((navItem, i) => {
                  if (navItem.url)
                    return (
                      <CommandItem
                        key={`${navItem.url}-${i}`}
                        value={navItem.title}
                        className='rounded-xl px-2.5 py-2'
                        onSelect={() => {
                          runCommand(() => navigate({ to: navItem.url }))
                        }}
                      >
                        <div className='bg-muted/50 flex size-6 items-center justify-center rounded-lg'>
                          <ArrowRight className='text-muted-foreground size-3' />
                        </div>
                        {navItem.title}
                      </CommandItem>
                    )

                  return navItem.items?.map((subItem, i) => (
                    <CommandItem
                      key={`${navItem.title}-${subItem.url}-${i}`}
                      value={`${navItem.title}-${subItem.url}`}
                      className='rounded-xl px-2.5 py-2'
                      onSelect={() => {
                        runCommand(() => navigate({ to: subItem.url }))
                      }}
                    >
                      <div className='bg-muted/50 flex size-6 items-center justify-center rounded-lg'>
                        <ArrowRight className='text-muted-foreground size-3' />
                      </div>
                      <span className='text-muted-foreground'>
                        {navItem.title}
                      </span>
                      <ChevronRight className='text-muted-foreground/60 size-3.5' />
                      <span>{subItem.title}</span>
                    </CommandItem>
                  ))
                })}
              </CommandGroup>
            ))}
            <CommandSeparator className='mx-2' />
            <CommandGroup heading={t('Theme')} className='p-1.5'>
              <CommandItem
                className='rounded-xl px-2.5 py-2'
                onSelect={() => runCommand(() => setTheme('light'))}
              >
                <div className='bg-muted/50 flex size-6 items-center justify-center rounded-lg'>
                  <Sun className='size-3.5' />
                </div>
                <span>{t('Light')}</span>
              </CommandItem>
              <CommandItem
                className='rounded-xl px-2.5 py-2'
                onSelect={() => runCommand(() => setTheme('dark'))}
              >
                <div className='bg-muted/50 flex size-6 items-center justify-center rounded-lg'>
                  <Moon className='size-3.5' />
                </div>
                <span>{t('Dark')}</span>
              </CommandItem>
              <CommandItem
                className='rounded-xl px-2.5 py-2'
                onSelect={() => runCommand(() => setTheme('system'))}
              >
                <div className='bg-muted/50 flex size-6 items-center justify-center rounded-lg'>
                  <Laptop className='size-3.5' />
                </div>
                <span>{t('System')}</span>
              </CommandItem>
            </CommandGroup>
          </ScrollArea>
        </CommandList>
      </Command>
    </CommandDialog>
  )
}
