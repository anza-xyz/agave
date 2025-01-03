
Pan ralladoagave
/CONTRIBUYENDO.md
Metadatos y controles de archivos

Avance

Código

Culpa
Pautas de codificación de Solana
El objetivo de estas pautas es mejorar la productividad de los desarrolladores al permitirles acceder a cualquier archivo del código base sin tener que adaptarse a las inconsistencias en la forma en que se escribe el código. El código base debe aparecer como si hubiera sido creado por un solo desarrollador. Si no está de acuerdo con una convención, envíe una solicitud de incorporación de cambios a este documento y ¡discutámoslo! Una vez que se acepta la solicitud de incorporación de cambios, todo el código debe actualizarse lo antes posible para reflejar las nuevas convenciones.

Solicitudes de extracción
Se prefieren los PR pequeños y frecuentes a los grandes y poco frecuentes. Un PR grande es difícil de revisar, puede impedir que otros hagan progresos y puede llevar rápidamente a su autor al "infierno del rebase". Un PR grande a menudo surge cuando un cambio requiere otro, que requiere otro, y luego otro. Cuando notes esas dependencias, coloca la solución en una confirmación propia, luego revisa una nueva rama y selecciónala.

$ git commit -am "Fix foo, needed by bar"
$ git checkout master
$ git checkout -b fix-foo
$ git cherry-pick fix-bar
$ git push --set-upstream origin fix-foo
Abra una solicitud de modificación para iniciar el proceso de revisión y luego vuelva a la rama original para seguir avanzando. Considere la posibilidad de realizar una reorganización para que su corrección sea la primera confirmación:

$ git checkout fix-bar
$ git rebase -i master <Move fix-foo to top>
Una vez que se fusiona la confirmación, vuelva a crear la rama original para purgar la confirmación seleccionada:

$ git pull --rebase upstream master
¿Qué tan grande es demasiado grande?
Si no hay cambios funcionales, las solicitudes de incorporación de cambios pueden ser muy extensas y eso no es un problema. Sin embargo, si sus cambios están generando cambios o adiciones significativas, entonces aproximadamente 1000 líneas de cambios es lo máximo que debe pedirle a un mantenedor de Solana que revise.

¿Debo enviar pequeñas solicitudes de relaciones públicas a medida que desarrollo componentes nuevos y grandes?
Agregue solo el código a la base de código que esté listo para implementarse. Si está creando una biblioteca grande, considere desarrollarla en un repositorio git separado. Cuando esté lista para integrarse, los encargados del mantenimiento de Solana Labs trabajarán con usted para decidir el camino a seguir. Las bibliotecas más pequeñas se pueden copiar, mientras que las muy grandes se pueden incorporar con un administrador de paquetes.

Cómo fusionar solicitudes de extracción
No hay una sola persona asignada para supervisar la cola de PR de GitHub y guiarte a través del proceso. Por lo general, le pedirás a la persona que escribió un componente que revise los cambios que se le hicieron. Puedes encontrar al autor usando git blameo preguntando en Discord. Cuando trabajes para fusionar tu PR, es muy importante entender que cambiar el código es tu prioridad y no necesariamente una prioridad de la persona de la que necesitas una aprobación. Además, si bien puedes interactuar más con el autor del componente, debes intentar ser incluyente con los demás. Proporcionar una descripción detallada del problema es el medio más eficaz para involucrar tanto al autor del componente como a otras partes potencialmente interesadas.

Considere abrir todas las PR como borradores de solicitudes de extracción primero. El uso de un borrador de PR le permite iniciar la automatización de la CI, que generalmente demora entre 10 y 30 minutos en ejecutarse. Use ese tiempo para escribir una descripción detallada del problema. Una vez que la descripción esté escrita y la CI se realice correctamente, haga clic en el botón "Listo para revisar" y agregue revisores. Agregar revisores antes de que la CI se realice correctamente es una vía rápida para perder la participación de los revisores. No solo recibirán una notificación y verán que la PR aún no está lista para ellos, sino que también serán bombardeados con notificaciones adicionales cada vez que envíe una confirmación para pasar la CI o hasta que "silencien" la PR. Una vez silenciada, deberá comunicarse por otro medio, como Discord, para solicitar que la revisen nuevamente. Cuando usa borradores de PR, no se envían notificaciones cuando envía confirmaciones y edita la descripción de la PR. Use borradores de PR con liberalidad. No moleste a los humanos hasta que haya superado a los bots.

¿Qué debe contener mi descripción de PR?
Revisar el código es una tarea ardua y, por lo general, implica intentar adivinar la intención del autor en distintos niveles. Asuma que el tiempo de los revisores es escaso y haga lo posible para que sus relaciones públicas sean lo más útiles posible. Inspiradas en técnicas para escribir buenos documentos técnicos, las pautas que se presentan aquí tienen como objetivo maximizar la participación de los revisores.

Suponga que el revisor no dedicará más de unos segundos a leer el título de la nota de prensa. Si no describe ningún cambio notable, no espere que el revisor haga clic para ver más.

A continuación, como si se tratara de un resumen de un informe técnico, el revisor dedicará unos 30 segundos a leer la descripción del problema de relaciones públicas. Si lo que se describe allí no parece más importante que los problemas en competencia, no espere que el revisor siga leyendo.

A continuación, el revisor leerá los cambios propuestos. En este punto, el revisor debe estar convencido de que los cambios propuestos son una buena solución al problema descrito anteriormente. Si los cambios propuestos, no los cambios en el código, generan debate, considere cerrar la solicitud de revisión y regresar con una propuesta de diseño.

Finalmente, una vez que el revisor comprende el problema y está de acuerdo con el enfoque para resolverlo, verá los cambios en el código. En este punto, el revisor simplemente busca ver si la implementación realmente implementa lo que se propuso y si esa implementación es sostenible. Cuando existe una prueba concisa y legible para cada nueva ruta de código, el revisor puede ignorar con seguridad los detalles de su implementación. Cuando faltan esas pruebas, es de esperar que se pierda la participación o que se obtenga una pila de comentarios de revisión a medida que el revisor intenta considerar cada ambigüedad en su implementación.

El título de relaciones públicas
El título de la RP debe contener un breve resumen del cambio, desde la perspectiva del usuario. Ejemplos de buenos títulos:

Añadir alquiler a las cuentas
Corregir error de falta de memoria en el validador
Limpiar process_message()en tiempo de ejecución
Las convenciones aquí son todas iguales a las de un buen título de confirmación de Git:

Primera palabra en mayúscula y en modo imperativo, no en tiempo pasado ("add", no "added")
Sin período de seguimiento
¿Qué se hizo, a quién se le hizo y en qué contexto?
El planteamiento del problema de las relaciones públicas
El repositorio git implementa un producto con varias características. El enunciado del problema debe describir cómo al producto le falta una característica, cómo una característica está incompleta o cómo la implementación de una característica es de alguna manera indeseable. Si un problema que se está solucionando ya describe el problema, continúe y cópielo y péguelo. Como se mencionó anteriormente, el tiempo del revisor es escaso. Dada una cola de solicitudes de revisión para revisar, el revisor puede ignorar las solicitudes de revisión que esperan que haga clic en los enlaces para ver si la solicitud de revisión merece atención.

Los cambios propuestos
Normalmente, el contenido de la sección "Cambios propuestos" será una lista con viñetas de los pasos que se tomaron para resolver el problema. A menudo, la lista es idéntica a las líneas de asunto de las confirmaciones de Git contenidas en la solicitud de modificación. Es especialmente generoso (y no se espera) reestructurar o reformular las confirmaciones de modo que cada cambio coincida con el flujo lógico en la descripción de la solicitud de modificación.

Las etiquetas de relaciones públicas/problemas
Las etiquetas facilitan la gestión y el seguimiento de las solicitudes de registro y los problemas. A continuación, se muestran algunas etiquetas comunes que utilizamos en Solana. Para obtener la lista completa de etiquetas, consulte la página de etiquetas :

"feature-gate": cuando agregue una nueva puerta de función o modifique el comportamiento de una puerta de función existente, agregue la etiqueta "feature-gate" a su PR. Las nuevas puertas de función también deben tener siempre un problema de seguimiento correspondiente (vaya a "Nuevo problema" -> " Comenzar con el rastreador de puertas de función ") y deben actualizarse cada vez que se active la función en un clúster.

"automerge": cuando una solicitud de incorporación de cambios tiene la etiqueta "automerge", se fusionará automáticamente una vez que se apruebe la integración continua. En general, esta etiqueta solo se debe utilizar para pequeñas correcciones urgentes (menos de 100 líneas) o solicitudes de incorporación de cambios generadas automáticamente. Si no está seguro, normalmente es que la solicitud de incorporación de cambios no está calificada como "automerge".

"Buen primer número": si encuentra un número que no es urgente y es autónomo con un alcance moderado, puede considerar adjuntarlo como "buen primer número", ya que puede ser una buena práctica para los recién llegados.

¿Cuándo se revisará mi PR?
Las solicitudes de incorporación de cambios se suelen revisar y fusionar en menos de 7 días. Si su solicitud de incorporación de cambios ha estado abierta durante más tiempo, es un fuerte indicador de que los revisores no están seguros de que el cambio cumpla con los estándares de calidad del código base. Puede considerar cerrarla y volver con solicitudes de incorporación de cambios más breves y descripciones más extensas que detallen qué problema resuelve y cómo lo resuelve. Las solicitudes de incorporación de cambios antiguas se marcarán como obsoletas y se cerrarán automáticamente 7 días después.

¿Cómo gestionar los comentarios de las reseñas?
Después de que un revisor brinde comentarios, puedes decir rápidamente "reconocido, lo solucionaremos" usando un emoji de pulgar hacia arriba. Si estás seguro de que la solución es exactamente la indicada, agrega una respuesta "Arreglado en COMMIT_HASH" y marca el comentario como resuelto. Si no estás seguro, responde "¿Es esto lo que tenías en mente? COMMIT_HASH" y, de ser así, el revisor responderá y marcará la conversación como resuelta. Marcar conversaciones como resueltas es una excelente manera de involucrar a más revisores. Dejar conversaciones abiertas puede implicar que la solicitud de revisión aún no está lista para una revisión adicional.

¿Cuándo se revisará nuevamente mi PR?
Recuerda que una vez que se abre tu solicitud de modificación, se envía una notificación cada vez que envías una confirmación. Una vez que un revisor agrega comentarios, no verificará el estado de esos comentarios después de cada nueva confirmación. En cambio, menciona directamente al revisor cuando consideres que tu solicitud de modificación está lista para otra revisión.

¿Es fácil decir "sí" a tus relaciones públicas?
Las solicitudes de relaciones públicas que son más fáciles de revisar tienen más probabilidades de ser revisadas. Esfuércese por hacer que sea fácil decir "sí" a sus solicitudes de relaciones públicas.

Lista no exhaustiva de cosas que dificultan la revisión:

Cambios adicionales que sean ortogonales al enunciado del problema y a los cambios propuestos. En lugar de eso, traslade esos cambios a una PR diferente.
Renombrar variables/funciones/tipos innecesariamente y/o sin explicación.
No seguir las convenciones establecidas en la función/módulo/cajón/repositorio.
Modificación de espacios en blanco: mover código y/o reformatear código. Realice dichos cambios en una solicitud de incorporación de cambios independiente.
Forzar el avance de la rama innecesariamente; esto hace más difícil rastrear cualquier comentario previo en líneas específicas de código, y también más difícil rastrear cambios ya revisados ​​de confirmaciones anteriores.
Cuando se requiere una implementación forzada (por ejemplo, para manejar un conflicto de fusión) y no se han realizado cambios nuevos desde la revisión anterior, indicarlo es beneficioso.
No responder a los comentarios de rondas de revisión anteriores. Siga las instrucciones en ¿Cómo gestionar los comentarios de las revisiones ?
Lista no exhaustiva de cosas que facilitan la revisión:

Agregar pruebas para todos los comportamientos nuevos o modificados.
Incluir en la descripción del PR cualquier prueba no automatizada que se haya realizado.
Incluir resultados relevantes para los cambios que apuntan a mejoras en el rendimiento.
Tenga en cuenta que estas listas son independientes de cuán simples o complicados sean los cambios de código reales .

Borradores de solicitudes de extracción
Si quieres recibir comentarios anticipados sobre tu PR, utiliza el mecanismo de "borrador de solicitud de incorporación de cambios" de GitHub. Los borradores de solicitudes de incorporación de cambios son una forma conveniente de colaborar con los encargados del mantenimiento de Solana sin activar notificaciones a medida que realizas cambios. Cuando sientas que tu PR está listo para una audiencia más amplia, puedes convertir tu borrador de solicitud de incorporación de cambios en una solicitud de incorporación de cambios estándar con solo hacer clic en un botón.

No agregue revisores a los borradores de PR. GitHub no borra automáticamente las aprobaciones cuando hace clic en "Listo para revisión", por lo que una revisión que significaba "Apruebo la dirección" de repente parece "Apruebo estos cambios". En su lugar, agregue un comentario que mencione los nombres de usuario de los que le gustaría una revisión. Pregunte explícitamente sobre qué le gustaría recibir comentarios.

Creación de cajas
Si su PR incluye un nuevo paquete, debe publicar su versión v0.0.1 antes de poder fusionarlo. Estos son los pasos:

Crea un subdirectorio para tu nueva caja.
En el directorio recién creado, cree un archivo Cargo.toml. A continuación, se muestra una plantilla de ejemplo:
[package]
name = "solana-<PACKAGE_NAME>"
version = "0.0.1"
description = "<DESCRIPTION>"
authors = ["Anza Maintainers <maintainers@anza.xyz>"]
repository = "https://github.com/anza-xyz/agave"
homepage = "https://anza.xyz"
documentation = "https://docs.rs/solana-<PACKAGE_NAME>"
license = "Apache-2.0"
edition = "2021"
Envíe la solicitud de incorporación de cambios para su revisión inicial. Debería ver que el trabajo de CI de verificación de caja falla porque la caja recién creada aún no se publicó.

Una vez que se hayan abordado todos los comentarios de la revisión, publique la versión v0.0.1 del paquete en su cuenta personal de crates.io y luego transfiera la propiedad del paquete a "anza-team". https://crates.io/policies#package-ownership

Después de una publicación exitosa, actualice la PR reemplazando el número de versión v0.0.1 con la versión correcta. En este momento, debería ver que el trabajo de CI de verificación de caja pasa y su caja publicada debería estar disponible en https://crates.io/crates/ .

Convenciones de codificación de Rust
Todo el código de Rust está formateado con la última versión de rustfmt. Una vez instalado, se actualizará automáticamente cuando actualice el compilador con rustup.

Todo el código de Rust está filtrado con Clippy. Si prefieres ignorar su consejo, hazlo explícitamente:

#[allow(clippy::too_many_arguments)]
Nota: Los valores predeterminados de Clippy se pueden anular en el archivo de nivel superior .clippy.toml.

En caso de duda, para los nombres de las variables, escríbalos con todas sus letras. La asignación de los nombres de tipo a los nombres de las variables consiste en poner en minúscula el nombre del tipo y poner un guión bajo antes de cada letra mayúscula. Los nombres de las variables no deben abreviarse a menos que se utilicen como argumentos de cierre y la brevedad mejore la legibilidad. Cuando una función tiene varias instancias del mismo tipo, califique cada una con un prefijo y un guión bajo (es decir, alice_keypair) o un sufijo numérico (es decir, tx0).

Para los nombres de funciones y métodos, utilice <verb>_<subject>. Para las pruebas unitarias, ese verbo siempre debe ser testy para las pruebas comparativas, el verbo siempre debe ser bench. Evite colocar espacios de nombres en los nombres de funciones con alguna palabra arbitraria. Evite abreviar palabras en los nombres de funciones.

Como dicen, "Cuando estés en Roma, haz lo que hacen los romanos". Un buen parche debería reconocer las convenciones de codificación del código que lo rodea, incluso en el caso de que ese código aún no se haya actualizado para cumplir con las convenciones descritas aquí.

Terminología
Se permite inventar nuevos términos, pero solo cuando el término sea ampliamente utilizado y comprendido. Evite introducir nuevos términos de tres letras, que pueden confundirse con acrónimos de tres letras.

Términos actualmente en uso

Propuestas de diseño
La arquitectura de este cliente validador de Solana se describe mediante documentos generados a partir de archivos Markdown en el docs/src/ directorio y que se pueden ver en el sitio web oficial de documentación del cliente validador de Solana Labs .

Las propuestas de diseño actuales se pueden ver en el sitio de documentos:

Propuestas aceptadas
Propuestas implementadas
Las nuevas propuestas de diseño deben seguir esta guía sobre cómo enviar una propuesta de diseño .
