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

import { importCodexAuth, importSub2apiData } from '../../api'
import { channelsQueryKeys } from '../../lib'

type ImportDataDialogProps = {
  open: boolean
  onOpenChange: (open: boolean) => void
}

type ImportMode = 'sub2api-data' | 'codex-auth'

export function ImportDataDialog(props: ImportDataDialogProps) {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const fileRef = useRef<HTMLInputElement>(null)
  const [mode, setMode] = useState<ImportMode>('sub2api-data')
  const [fileName, setFileName] = useState('')
  const [content, setContent] = useState('')
  const [group, setGroup] = useState('default')
  const [isSubmitting, setIsSubmitting] = useState(false)

  const reset = () => {
    setFileName('')
    setContent('')
    setGroup('default')
    if (fileRef.current) fileRef.current.value = ''
  }

  const handleFile = async (file: File | null) => {
    if (!file) return
    if (!file.name.toLowerCase().endsWith('.json') && mode === 'sub2api-data') {
      toast.error(t('Please select a JSON (.json) data file'))
      return
    }
    setFileName(file.name)
    const text = await file.text()
    setContent(text)
  }

  const handleImport = async () => {
    if (!content.trim()) {
      toast.error(t('Please select a data file'))
      return
    }
    setIsSubmitting(true)
    try {
      if (mode === 'sub2api-data') {
        const result = await importSub2apiData({
          content,
          group: group.trim() || 'default',
        })
        if (!result.success || !result.data) {
          toast.error(result.message || t('Import failed'))
          return
        }
        const d = result.data
        toast.success(
          t(
            'Import done: proxies +{{createdP}}/reuse {{reusedP}}, accounts +{{createdA}}, failed P{{failedP}}/A{{failedA}}',
            {
              createdP: d.proxy_created,
              reusedP: d.proxy_reused,
              createdA: d.account_created,
              failedP: d.proxy_failed,
              failedA: d.account_failed,
            }
          )
        )
        if (d.errors?.length) {
          toast.message(
            t('{{count}} import warnings/errors — check server logs', {
              count: d.errors.length,
            })
          )
        }
      } else {
        const result = await importCodexAuth({
          content,
          group: group.trim() || 'default',
          update_existing: true,
        })
        if (!result.success || !result.data) {
          toast.error(result.message || t('Import failed'))
          return
        }
        const d = result.data
        toast.success(
          t(
            'Codex import: created {{created}}, updated {{updated}}, skipped {{skipped}}, failed {{failed}}',
            {
              created: d.created,
              updated: d.updated,
              skipped: d.skipped,
              failed: d.failed,
            }
          )
        )
      }
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
          <DialogTitle>{t('Import Data')}</DialogTitle>
          <DialogDescription>
            {t(
              'Upload an exported JSON file to bulk-import accounts and proxies. New accounts and proxies will be created; bind groups manually. Ensure data will not conflict with existing records.'
            )}
          </DialogDescription>
        </DialogHeader>

        <div className='space-y-4'>
          <div className='space-y-2'>
            <Label>{t('Import type')}</Label>
            <div className='flex gap-2'>
              <Button
                type='button'
                size='sm'
                variant={mode === 'sub2api-data' ? 'default' : 'outline'}
                onClick={() => setMode('sub2api-data')}
              >
                {t('Sub2API data JSON')}
              </Button>
              <Button
                type='button'
                size='sm'
                variant={mode === 'codex-auth' ? 'default' : 'outline'}
                onClick={() => setMode('codex-auth')}
              >
                {t('Codex auth file')}
              </Button>
            </div>
          </div>

          <div className='space-y-2'>
            <Label>{t('Data file')}</Label>
            <p className='text-muted-foreground text-xs'>
              {mode === 'sub2api-data'
                ? t('JSON (.json) — sub2api export (proxies + accounts)')
                : t('Codex / sub2api session auth JSON or access token paste')}
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
                {fileName || t('No file selected')}
              </span>
              <input
                ref={fileRef}
                type='file'
                accept={
                  mode === 'sub2api-data' ? '.json,application/json' : '*/*'
                }
                className='hidden'
                onChange={(e) => {
                  void handleFile(e.target.files?.[0] ?? null)
                }}
              />
            </div>
            {content ? (
              <div className='bg-muted/40 flex items-center gap-2 rounded-md border px-3 py-2 text-xs'>
                <FileJson className='h-4 w-4 shrink-0' />
                <span>
                  {t('{{bytes}} bytes loaded', {
                    bytes: new Blob([content]).size,
                  })}
                </span>
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
            disabled={isSubmitting || !content.trim()}
          >
            {isSubmitting ? t('Importing...') : t('Import')}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
