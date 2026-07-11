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
import { useQueryClient } from '@tanstack/react-query'
import { FileJson, Upload } from 'lucide-react'
import { useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { toast } from 'sonner'

import { Button } from '@/components/ui/button'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'

import { importAuthJson, importAuthUpload } from '../../api'
import { channelsQueryKeys } from '../../lib'

type ImportDataDialogProps = {
  open: boolean
  onOpenChange: (open: boolean) => void
}

/** auto = detect; or force a family */
type ImportMode = 'auto' | 'sub2api-data' | 'cliproxy' | 'codex-session'

function fileHelpText(
  mode: ImportMode,
  t: (key: string) => string
): string {
  if (mode === 'sub2api-data') {
    return t('JSON (.json) — sub2api export (proxies + accounts)')
  }
  if (mode === 'cliproxy') {
    return t(
      'One or more CLIProxyAPI *.json auth files (type: codex/claude/gemini)'
    )
  }
  if (mode === 'codex-session') {
    return t('Codex / sub2api session auth JSON or access token')
  }
  return t(
    'Auto: sub2api-data export, CLIProxyAPI auth files, or Codex session'
  )
}

export function ImportDataDialog(props: ImportDataDialogProps) {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const fileRef = useRef<HTMLInputElement>(null)
  const [mode, setMode] = useState<ImportMode>('auto')
  const [files, setFiles] = useState<File[]>([])
  const [group, setGroup] = useState('default')
  const [isSubmitting, setIsSubmitting] = useState(false)

  const reset = () => {
    setFiles([])
    setGroup('default')
    if (fileRef.current) fileRef.current.value = ''
  }

  const handleFiles = (list: FileList | null) => {
    if (!list?.length) return
    const next = Array.from(list)
    setFiles(next)
  }

  const summarize = (data: {
    format?: string
    channels?: {
      created?: number
      updated?: number
      skipped?: number
      failed?: number
    }
    data?: {
      proxy_created?: number
      proxy_reused?: number
      account_created?: number
      account_failed?: number
      proxy_failed?: number
    }
    file_results?: Array<{ ok: boolean; name: string; message?: string }>
  }) => {
    if (data.data) {
      const d = data.data
      toast.success(
        t(
          'Import done: proxies +{{createdP}}/reuse {{reusedP}}, accounts +{{createdA}}, failed P{{failedP}}/A{{failedA}}',
          {
            createdP: d.proxy_created ?? 0,
            reusedP: d.proxy_reused ?? 0,
            createdA: d.account_created ?? 0,
            failedP: d.proxy_failed ?? 0,
            failedA: d.account_failed ?? 0,
          }
        )
      )
      return
    }
    const c = data.channels
    if (c) {
      toast.success(
        t(
          'Auth import ({{format}}): created {{created}}, updated {{updated}}, skipped {{skipped}}, failed {{failed}}',
          {
            format: data.format || 'auto',
            created: c.created ?? 0,
            updated: c.updated ?? 0,
            skipped: c.skipped ?? 0,
            failed: c.failed ?? 0,
          }
        )
      )
    }
    const bad = data.file_results?.filter((f) => !f.ok) ?? []
    if (bad.length) {
      toast.message(
        t('{{count}} file(s) had errors', { count: bad.length })
      )
    }
  }

  const handleImport = async () => {
    if (!files.length) {
      toast.error(t('Please select a data file'))
      return
    }
    setIsSubmitting(true)
    try {
      const format = mode === 'auto' ? 'auto' : mode
      const useMultipart = files.length > 1 || mode === 'cliproxy'

      const result = useMultipart
        ? await importAuthUpload({
            files,
            format,
            group: group.trim() || 'default',
            update_existing: true,
          })
        : await importAuthJson({
            format,
            content: await files[0].text(),
            filenames: [files[0].name],
            group: group.trim() || 'default',
            update_existing: true,
          })

      if (!result.success || !result.data) {
        toast.error(result.message || t('Import failed'))
        return
      }
      summarize(result.data)
      await queryClient.invalidateQueries({
        queryKey: channelsQueryKeys.lists(),
      })
      props.onOpenChange(false)
      reset()
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('Import failed'))
    } finally {
      setIsSubmitting(false)
    }
  }

  return (
    <Dialog
      open={props.open}
      onOpenChange={(open) => {
        props.onOpenChange(open)
        if (!open) reset()
      }}
    >
      <DialogContent className='sm:max-w-lg'>
        <DialogHeader>
          <DialogTitle>{t('Import credentials')}</DialogTitle>
          <DialogDescription>
            {t(
              'Import Sub2API exports, CLIProxyAPI auth JSON files, or Codex session tokens. Multiple files supported. Groups are not auto-bound — set a default group below if needed.'
            )}
          </DialogDescription>
        </DialogHeader>

        <div className='space-y-4'>
          <div className='space-y-2'>
            <Label>{t('Format')}</Label>
            <div className='flex flex-wrap gap-2'>
              {(
                [
                  ['auto', t('Auto-detect')],
                  ['sub2api-data', t('Sub2API data JSON')],
                  ['cliproxy', t('CLIProxyAPI auth')],
                  ['codex-session', t('Codex session')],
                ] as const
              ).map(([value, label]) => (
                <Button
                  key={value}
                  type='button'
                  size='sm'
                  variant={mode === value ? 'default' : 'outline'}
                  onClick={() => setMode(value)}
                >
                  {label}
                </Button>
              ))}
            </div>
          </div>

          <div className='space-y-2'>
            <Label>{t('Data file')}</Label>
            <p className='text-muted-foreground text-xs'>
              {fileHelpText(mode, t)}
            </p>
            <div className='flex items-center gap-2'>
              <Button
                type='button'
                variant='outline'
                size='sm'
                onClick={() => fileRef.current?.click()}
              >
                <Upload className='mr-2 h-4 w-4' />
                {t('Choose file')}
              </Button>
              <span className='text-muted-foreground truncate text-sm'>
                {files.length === 0
                  ? t('No file selected')
                  : files.length === 1
                    ? files[0].name
                    : t('{{count}} files selected', { count: files.length })}
              </span>
              <input
                ref={fileRef}
                type='file'
                multiple
                accept='.json,application/json,text/plain,*/*'
                className='hidden'
                onChange={(e) => {
                  handleFiles(e.target.files)
                }}
              />
            </div>
            {files.length > 0 ? (
              <div className='bg-muted/40 max-h-28 space-y-1 overflow-auto rounded-md border px-3 py-2 text-xs'>
                {files.map((f) => (
                  <div key={f.name + f.size} className='flex items-center gap-2'>
                    <FileJson className='h-3.5 w-3.5 shrink-0' />
                    <span className='truncate'>
                      {f.name} ({f.size} B)
                    </span>
                  </div>
                ))}
              </div>
            ) : null}
          </div>

          <div className='space-y-2'>
            <Label htmlFor='import-group'>{t('Default group')}</Label>
            <Input
              id='import-group'
              value={group}
              onChange={(e) => setGroup(e.target.value)}
              placeholder='default'
            />
            <p className='text-muted-foreground text-xs'>
              {t(
                'Applied to new channels. Rebind groups in channel settings if needed.'
              )}
            </p>
          </div>
        </div>

        <DialogFooter>
          <Button
            type='button'
            variant='outline'
            onClick={() => props.onOpenChange(false)}
            disabled={isSubmitting}
          >
            {t('Cancel')}
          </Button>
          <Button
            type='button'
            onClick={() => {
              void handleImport()
            }}
            disabled={isSubmitting || files.length === 0}
          >
            {isSubmitting ? t('Importing...') : t('Import')}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
